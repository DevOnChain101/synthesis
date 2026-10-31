//! `#` notes and `/memory` in the TUI: the person's own, confirmed append to an instruction file.
//! Nothing here is automatic and nothing here runs for the model.

use std::io::Write;
use std::path::{Path, PathBuf};

use synthesis_context::InstructionFile;
use synthesis_events::Redactor;

const NOTE_CAP: usize = 2048;

/// What the TUI needs to save `#` notes and to answer `/memory`.
pub struct Notes {
    pub user: PathBuf,
    pub project: PathBuf,
    pub root: PathBuf,
    pub secrets: Vec<String>,
    pub loaded: Vec<String>,
}

/// User: `$SYNTHESIS_HOME/SYNTHESIS.md`. Project: the root's `SYNTHESIS.md` or `AGENTS.md` when one exists
/// (the files the loader prefers), else `<root>/.synthesis/SYNTHESIS.md`, which is loaded next to them.
pub fn project_file(root: &Path) -> PathBuf {
    ["SYNTHESIS.md", "AGENTS.md"]
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| root.join(".synthesis").join("SYNTHESIS.md"))
}

pub fn describe(files: &[InstructionFile]) -> Vec<String> {
    files
        .iter()
        .map(|f| format!("  {} ({} bytes)", f.path.display(), f.content.len()))
        .collect()
}

impl Notes {
    pub fn hook(self) -> synthesis_tui::MemoryHook {
        let report = {
            let mut v = vec!["Instruction files loaded at start:".to_string()];
            v.extend(self.loaded.iter().cloned());
            if self.loaded.is_empty() {
                v.push("  (none)".into());
            }
            v.push(format!("# note, user file:    {}", self.user.display()));
            v.push(format!("# note, project file: {}", self.project.display()));
            v.push("Type # then your note to save it (you confirm the file first). Notes apply to new sessions.".into());
            v
        };
        let shown = |p: &Path| p.display().to_string();
        synthesis_tui::MemoryHook {
            user_file: shown(&self.user),
            project_file: shown(&self.project),
            report: Box::new(move || report.clone()),
            preview: {
                let secrets = self.secrets.clone();
                std::rc::Rc::new(move |text| final_note(text, &Redactor::new(secrets.clone())).0)
            },
            save: Box::new(move |user, text| {
                let (path, root) = if user {
                    (&self.user, None)
                } else {
                    (&self.project, Some(&self.root))
                };
                let masked = append_note(path, root, text, &Redactor::new(self.secrets.clone()))?;
                Ok(format!(
                    "saved to {}{} · applies to new sessions",
                    path.display(),
                    if masked { " (secrets masked)" } else { "" }
                ))
            }),
        }
    }
}

/// What a note becomes: one line, secrets masked, and every `@token` in backticks so it can never
/// be expanded as an `@import` when the file is loaded. The flag says whether something was masked.
pub fn final_note(text: &str, redactor: &Redactor) -> (String, bool) {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = redactor.redact_str(&one_line);
    let masked = redacted != one_line;
    let note = redacted
        .split(' ')
        .map(|t| {
            if t.starts_with('@') {
                format!("`{t}`")
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    (note, masked)
}

/// Appends `- <note>` (see [`final_note`], at most 2 KiB) to `path`; `Ok(true)` when something
/// was masked. A project file must stay inside `root` after symlinks (checked before anything is
/// created), must not be a symlink itself and must not lie under `.git/`, so a repository cannot
/// point `AGENTS.md` at some other file and have the note land there. The file is opened with
/// `O_NOFOLLOW` and mode 0600 on unix.
pub fn append_note(
    path: &Path,
    root: Option<&PathBuf>,
    text: &str,
    redactor: &Redactor,
) -> Result<bool, String> {
    let (note, masked) = final_note(text, redactor);
    if note.is_empty() || note.len() > NOTE_CAP {
        return Err(format!("a note must be 1 to {NOTE_CAP} bytes"));
    }
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(root) = root {
        let real =
            synthesis_core::realpath_lenient(path).ok_or("a dangling symlink is in the way")?;
        let real_root = synthesis_core::canonicalize(root).map_err(io)?;
        if !synthesis_core::contains(&real_root, &real, synthesis_core::PathPolicy::NATIVE) {
            return Err(format!("{} resolves outside the project", path.display()));
        }
        if real
            .strip_prefix(&real_root)
            .is_ok_and(|r| r.components().any(|c| c.as_os_str() == ".git"))
        {
            return Err(format!("{} is inside .git", path.display()));
        }
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!(
            "{} is a symbolic link; not writing",
            path.display()
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    o.open(path)
        .and_then(|mut f| f.write_all(format!("{sep}- {note}\n").as_bytes()))
        .map_err(io)?;
    Ok(masked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Redactor {
        Redactor::new(["hunter2hunter2".to_string()])
    }

    #[test]
    fn project_file_prefers_existing_synthesis_then_agents_else_dot_synthesis() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        assert_eq!(project_file(p), p.join(".synthesis/SYNTHESIS.md"));
        std::fs::write(p.join("AGENTS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("AGENTS.md"));
        std::fs::write(p.join("SYNTHESIS.md"), "x").unwrap();
        assert_eq!(project_file(p), p.join("SYNTHESIS.md"));
    }

    #[test]
    fn note_is_appended_on_its_own_line_with_secrets_masked() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(".synthesis/SYNTHESIS.md");
        let root = synthesis_core::canonicalize(d.path()).unwrap();
        assert_eq!(
            append_note(&f, Some(&root), "run cargo test\nfirst", &r()),
            Ok(false)
        );
        std::fs::write(&f, "# Rules\n- old").unwrap(); // no trailing newline
        let masked = append_note(
            &f,
            Some(&root),
            "key is hunter2hunter2 and sk-ant-abc123",
            &r(),
        );
        assert_eq!(masked, Ok(true));
        let t = std::fs::read_to_string(&f).unwrap();
        assert_eq!(
            t,
            "# Rules\n- old\n- key is [REDACTED:secret] and [REDACTED:sk-ant]\n"
        );
        assert!(append_note(&f, Some(&root), "   ", &r()).is_err());
        assert!(append_note(&f, Some(&root), &"x".repeat(NOTE_CAP + 1), &r()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn project_file_symlinked_outside_the_project_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (root, other) = (d.path().join("repo"), d.path().join("elsewhere"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&other, "target\n").unwrap();
        std::os::unix::fs::symlink(&other, root.join("AGENTS.md")).unwrap();
        let root = synthesis_core::canonicalize(&root).unwrap();
        let e = append_note(&root.join("AGENTS.md"), Some(&root), "note", &r()).unwrap_err();
        assert!(e.contains("outside the project"), "{e}");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "target\n");
    }

    #[test]
    fn at_tokens_are_defanged_and_the_final_note_is_what_the_preview_shows() {
        let (n, masked) = final_note("see @~/.ssh/id_rsa and @x.md\nbye a@b", &r());
        assert_eq!(n, "see `@~/.ssh/id_rsa` and `@x.md` bye a@b");
        assert!(!masked);
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("SYNTHESIS.md");
        let root = synthesis_core::canonicalize(d.path()).unwrap();
        append_note(&f, Some(&root), "@~/.env", &r()).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "- `@~/.env`\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_git_targets_are_refused_and_nothing_is_created_outside() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let root = synthesis_core::canonicalize(d.path()).unwrap().join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let other = d.path().join("other");
        std::fs::write(&other, "t\n").unwrap();
        // a symlink that stays inside the project is still refused
        std::fs::write(root.join("real.md"), "x\n").unwrap();
        std::os::unix::fs::symlink(root.join("real.md"), root.join("AGENTS.md")).unwrap();
        let e = append_note(&root.join("AGENTS.md"), Some(&root), "n", &r()).unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        // under .git/
        let e = append_note(&root.join(".git/SYNTHESIS.md"), Some(&root), "n", &r()).unwrap_err();
        assert!(e.contains(".git"), "{e}");
        // a directory symlinked out of the project: refused BEFORE create_dir_all makes anything
        std::os::unix::fs::symlink(d.path().join("out"), root.join("esc")).unwrap();
        let e =
            append_note(&root.join("esc/sub/SYNTHESIS.md"), Some(&root), "n", &r()).unwrap_err();
        assert!(e.contains("outside") || e.contains("dangling"), "{e}");
        assert!(!d.path().join("out").exists());
        // new files are private
        let f = root.join(".synthesis/SYNTHESIS.md");
        append_note(&f, Some(&root), "n", &r()).unwrap();
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
