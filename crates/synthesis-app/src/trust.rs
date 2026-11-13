//! Project trust as the engine decides it: the refusal rules, the remembered-trust lookup in the user
//! store (MAC-checked rows, `synthesis_session::trust`), the launch flags a trust produces and the
//! `TrustCheck` that compares the trusted configuration again before anything of it runs. Shared by the
//! desktop window (local engine) and `synt engine --stdio` (the engine inside WSL decides, never the client).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use synthesis_harness::sanitize_for_display;

use crate::run::Common;

/// A session-only grant: the root it was given for, the manifest of the trusted configuration (checked again at
/// every start; `None`: not fingerprintable) and whether it applies project execution (`false`: remembered, inert).
pub type Grant = (PathBuf, Option<String>, bool);

/// Canonical project root of `cwd` (what `--trust-project` was granted for).
pub fn root_of(cwd: &Path) -> PathBuf {
    let r = synthesis_settings::find_project_root(cwd);
    synthesis_core::canonicalize(&r).unwrap_or(r)
}

/// `--trust-project` / `--approve-mcp` were granted for ONE project root. After a switch to another
/// root they are dropped (untrusted, no re-prompt: relaunch with --cwd for that project).
pub fn common_for(c: &Common, granted: &Path, cwd: &Path) -> (Common, Option<String>) {
    let mut c = c.clone();
    let mut note = None;
    if c.trust_project && root_of(cwd) != granted {
        note = Some(format!(
            "project root changed; --trust-project and --approve-mcp apply only to {}, so this project is untrusted",
            sanitize_for_display(&granted.display().to_string())
        ));
        c.trust_project = false;
        c.approve_mcp.clear();
    }
    (c, note)
}

/// The refusal rules of `synthesis_session::trust` for the folder a session trust would grant.
pub fn trust_policy(home: Option<&Path>, synt_home: &Path) -> synthesis_session::trust::Policy {
    synthesis_session::trust::Policy {
        home: home.map(Path::to_path_buf),
        synt_home: synt_home.to_path_buf(),
    }
}

fn refusal_text(what: &str, root: &Path) -> String {
    format!(
        "trust not offered: it would trust {what} ({}). Trust a specific project folder instead: start Synthesis in it with --trust-project.",
        sanitize_for_display(&root.display().to_string())
    )
}

/// Why `root` (the folder a session trust would grant) must not be trusted, if so: a filesystem root, the
/// user's home or a parent of it, or Synthesis's own config folder (or inside it). Such a folder is trusted
/// only by starting in a specific project folder.
pub fn trust_refusal(root: &Path, home: Option<&Path>, synt_home: &Path) -> Option<String> {
    trust_policy(home, synt_home)
        .root_refusal(root)
        .map(|w| refusal_text(w, root))
}

/// The same for a PARENT grant ("all its subfolders"): also the generic containers (temp folders, Downloads,
/// Desktop, Documents, world-writable and root-owned system folders).
pub fn parent_refusal(parent: &Path, home: Option<&Path>, synt_home: &Path) -> Option<String> {
    trust_policy(home, synt_home)
        .parent_refusal(parent)
        .map(|w| refusal_text(w, parent))
}

/// The flags of a session the person trusted in the Trust dialog: `--trust-project`, and NO `--approve-mcp`
/// carried over from the launch (an MCP server is approved by name and hash, never by an earlier flag).
pub fn session_trusted(c: &Common) -> Common {
    let mut c = c.clone();
    c.trust_project = true;
    c.approve_mcp.clear();
    c.project_exec_inert = false;
    c
}

/// The flags of a REMEMBERED trust (and of "Trust this folder (remember)" for the run that follows it): only the
/// project's DATA applies (agents, skills, memory, instruction files, the file explorer). Its settings and
/// provider config load with the untrusted filter (no hooks, MCP servers, allow rules, env, defaultMode,
/// additionalDirectories, verification checks or provider connection). Only a trust given for this run
/// (session-only, or `--trust-project`) applies those.
pub fn remembered_trusted(c: &Common) -> Common {
    let mut c = session_trusted(c);
    c.project_exec_inert = true;
    c
}

/// A path as shown to the person: control characters made visible.
pub fn display_path(p: &Path) -> String {
    sanitize_for_display(&p.display().to_string())
}

/// What the USER store says about `project` (never a project file, never anything a tool wrote: rows are
/// MAC-checked in `synthesis_session::trust`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remembered {
    /// No remembered trust covers it.
    No,
    /// Trusted, and the project's trusted configuration is what it was (the manifest that was checked).
    Trusted(String),
    /// Trusted once, but the configuration differs (or cannot be fingerprinted now): NOT honoured. The files
    /// that differ, and when it was trusted.
    Changed { files: Vec<String>, since: String },
}

/// Looks `project` up. A `folder` record is compared with its own manifest; a `parent` record is compared with
/// the baseline of this project, which the first start under that grant records (`write_baseline`). A store
/// that cannot be read trusts nothing.
pub fn remembered_trust(
    synt_home: &Path,
    user_home: Option<&Path>,
    project: &Path,
    write_baseline: bool,
) -> Remembered {
    use synthesis_session::trust::{self, FOLDER};
    let Ok(listing) = trust::list(synt_home) else {
        return Remembered::No;
    };
    let policy = trust_policy(user_home, synt_home);
    let root = root_of(project);
    let Some(rec) = trust::covers(&listing.records, &root, &policy) else {
        return Remembered::No;
    };
    let now = trust::fingerprint(&root, project);
    let since = rec
        .trusted_at
        .get(..10)
        .unwrap_or(&rec.trusted_at)
        .to_string();
    let changed = |old: &str| match &now {
        Ok(n) if n == old => Remembered::Trusted(n.clone()),
        Ok(n) => Remembered::Changed {
            files: trust::changed_files(old, n),
            since: since.clone(),
        },
        Err(e) => Remembered::Changed {
            files: vec![e.to_string()],
            since: since.clone(),
        },
    };
    if rec.scope == FOLDER {
        return changed(&rec.fingerprint);
    }
    match trust::child_of(&listing.records, &root, &policy) {
        Some(c) => changed(&c.fingerprint),
        None => {
            // the first start of this project under the parent grant: it is the baseline from now on
            match &now {
                // a baseline that cannot be written is no baseline: not trusted
                Ok(n)
                    if write_baseline
                        && trust::add(synt_home, &root, trust::CHILD, n, &policy).is_err() =>
                {
                    Remembered::No
                }
                Ok(n) => Remembered::Trusted(n.clone()),
                Err(_) => changed(""),
            }
        }
    }
}

/// The flags of one launch and its notices: the session-trusted ones when `trust` names the root of `project`;
/// else the launch flags as `common_for` leaves them, plus trust when the user store remembers the project, its
/// trusted configuration is unchanged and the sandbox is enforcing (a shell that can run unconfined could have
/// written the store; session-only trust and `--trust-project` still work then). A changed configuration starts
/// the project untrusted with a notice naming the files; /trust ask asks again.
pub fn launch_common(
    c: &Common,
    granted: &Path,
    trust: Option<&(PathBuf, Option<String>, bool)>,
    remembered: bool,
    project: &Path,
    synt_home: &Path,
    user_home: Option<&Path>,
) -> (Common, Vec<String>, Option<TrustCheck>) {
    let check = |root: &Path, manifest: String| TrustCheck {
        root: root.to_path_buf(),
        cwd: project.to_path_buf(),
        manifest,
        before_compare: None,
    };
    if let Some((root, manifest, exec)) = trust.filter(|(root, _, _)| root_of(project) == *root) {
        let chk = manifest.clone().map(|m| check(root, m));
        let c = if *exec {
            session_trusted(c)
        } else {
            remembered_trusted(c)
        };
        return (c, vec![], chk);
    }
    let (mut c, note) = common_for(c, granted, project);
    let mut notes: Vec<String> = note.into_iter().collect();
    if c.trust_project || !remembered {
        return (c, notes, None);
    }
    let enforcing = c.sandbox_mode() == synthesis_terminal::SandboxMode::Enforce;
    let root = root_of(project);
    let shown = sanitize_for_display(&root.display().to_string());
    let mut chk = None;
    match remembered_trust(synt_home, user_home, project, enforcing) {
        Remembered::No => {}
        _ if !enforcing => notes.push(format!(
            "remembered trust of {shown} is not honoured: the sandbox is not enforcing (a command that can run unconfined could have written it); start with --sandbox enforce, or trust the folder for this session only"
        )),
        Remembered::Trusted(m) => {
            c = remembered_trusted(&c);
            chk = Some(check(&root, m));
        }
        Remembered::Changed { files, since } => notes.push(format!(
            "project config changed since you trusted it on {since} ({}): {shown} starts untrusted; review the files, then run /trust ask to trust it again",
            sanitize_for_display(&files.join(", "))
        )),
    }
    (c, notes, chk)
}

/// Marks the error of a start that was refused because the trusted configuration changed meanwhile.
pub const TRUST_CHANGED: &str = "trust-changed";

pub fn trust_changed_error(files: &[String]) -> String {
    format!(
        "{TRUST_CHANGED}: the trusted configuration changed while the session was starting ({})",
        sanitize_for_display(&files.join(", "))
    )
}

/// What was approved: the manifest of `root`'s trusted configuration (session folder `cwd`) when the trust was
/// decided or stored.
#[derive(Clone)]
pub struct TrustCheck {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub manifest: String,
    /// Runs just before the comparison (tests change a file here, as a racing writer would).
    pub before_compare: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl TrustCheck {
    /// `Err(files)` when the configuration on disk is no longer the approved one (or cannot be fingerprinted).
    pub fn verify(&self) -> Result<(), Vec<String>> {
        if let Some(f) = &self.before_compare {
            f();
        }
        match synthesis_session::trust::fingerprint(&self.root, &self.cwd) {
            Ok(now) if now == self.manifest => Ok(()),
            Ok(now) => Err(synthesis_session::trust::changed_files(
                &self.manifest,
                &now,
            )),
            Err(e) => Err(vec![e.to_string()]),
        }
    }
}
