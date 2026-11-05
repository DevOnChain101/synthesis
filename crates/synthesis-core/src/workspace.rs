use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};

/// Decided once at creation; comparison only, stored paths are never rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathPolicy {
    pub case_insensitive: bool,
    pub windows_separators: bool,
}

impl PathPolicy {
    pub const NATIVE: PathPolicy = PathPolicy {
        case_insensitive: cfg!(any(windows, target_os = "macos")),
        windows_separators: cfg!(windows),
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceContext {
    pub workspace_id: String,
    /// Stable across sessions (hash of the root); `workspace_id` is per session. Empty in data
    /// written before it existed.
    #[serde(default)]
    pub workspace_key: String,
    #[serde(default)]
    pub workspace_label: String,
    /// Shared by all git worktrees of one repository.
    #[serde(default)]
    pub repo_key: String,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub git_worktree: Option<PathBuf>,
    pub artifact_root: PathBuf,
    pub browser_context_id: Option<String>,
    pub process_group_id: String,
    pub path_policy: PathPolicy,
}

#[derive(Debug)]
pub enum ResolveError {
    /// Not lexically inside `cwd` or any additional dir.
    Outside(PathBuf),
    /// Lexically inside, but a symlink leads outside (or is dangling).
    OutsideViaLink(PathBuf),
    /// UNC `\\host\share`: never inside, always needs approval.
    Network(PathBuf),
    Io(io::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outside(p) => write!(f, "path outside workspace: {}", p.display()),
            Self::OutsideViaLink(p) => {
                write!(f, "path escapes workspace via symlink: {}", p.display())
            }
            Self::Network(p) => write!(f, "network path not allowed: {}", p.display()),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for ResolveError {}

impl WorkspaceContext {
    /// Builds a context from explicit selections; `root`, `cwd` and extra dirs are stored as
    /// realpaths (macOS `/var` vs `/private/var`). `cwd` must be inside `root` or an extra dir.
    pub fn open(
        workspace_id: impl Into<String>,
        root: &Path,
        cwd: &Path,
        additional_dirs: &[PathBuf],
        artifact_root: PathBuf,
        process_group_id: impl Into<String>,
    ) -> Result<Self, ResolveError> {
        let real = |p: &Path| canonicalize(p).map_err(ResolveError::Io);
        let root = real(root)?;
        let cwd = real(cwd)?;
        let additional_dirs = additional_dirs
            .iter()
            .map(|p| real(p))
            .collect::<Result<Vec<_>, _>>()?;
        let path_policy = PathPolicy::NATIVE;
        let ok = std::iter::once(&root)
            .chain(&additional_dirs)
            .any(|r| contains(r, &cwd, path_policy));
        if !ok {
            return Err(ResolveError::Outside(cwd));
        }
        Ok(Self {
            workspace_id: workspace_id.into(),
            workspace_key: crate::workspace_key(&root, path_policy),
            workspace_label: crate::workspace_label(&root),
            repo_key: crate::repo_key(&root, path_policy),
            root,
            cwd,
            additional_dirs,
            git_worktree: None,
            artifact_root,
            browser_context_id: None,
            process_group_id: process_group_id.into(),
            path_policy,
        })
    }

    /// Resolves a tool path (spec §4.2) to a lexically normalised absolute path that is
    /// guaranteed inside `cwd` or an additional dir, also after symlink resolution.
    /// `~` is NOT expanded (`~/x` is the relative path `<cwd>/~/x`); `\` is never rewritten on POSIX.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, ResolveError> {
        let policy = self.path_policy;
        if policy.windows_separators
            && (input.starts_with("\\\\") || input.starts_with("//"))
            && !(input.starts_with("\\\\?\\") || input.starts_with("\\\\.\\"))
        {
            return Err(ResolveError::Network(PathBuf::from(input)));
        }
        let p = PathBuf::from(input);
        let mut abs = lexical_normalize(&if p.is_absolute() { p } else { self.cwd.join(p) });

        let find = |a: &Path| {
            std::iter::once(&self.cwd)
                .chain(&self.additional_dirs)
                .find(|r| contains(r, a, policy))
        };
        // Roots are realpaths: a path spelled through a symlinked prefix of a root (macOS `/tmp`)
        // is judged, and returned, by its realpath.
        let root = match find(&abs) {
            Some(r) => r,
            None => {
                let real =
                    realpath_lenient(&abs).ok_or_else(|| ResolveError::Outside(abs.clone()))?;
                let r = find(&real).ok_or_else(|| ResolveError::Outside(abs.clone()))?;
                abs = real;
                r
            }
        };

        if policy.windows_separators
            && let Some(bad) = abs
                .components()
                .skip(root.components().count())
                .filter_map(|c| match c {
                    Component::Normal(n) => n.to_str(),
                    _ => None,
                })
                .find(|n| is_windows_reserved(n))
        {
            return Err(ResolveError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("reserved or unsafe Windows file name: {bad:?}"),
            )));
        }

        let real =
            realpath_lenient(&abs).ok_or_else(|| ResolveError::OutsideViaLink(abs.clone()))?;
        // Roots are realpaths already (see `open`), so compare directly.
        if contains(root, &real, policy) {
            Ok(abs)
        } else {
            Err(ResolveError::OutsideViaLink(abs))
        }
    }
}

/// Windows device names (CON, NUL, COM1..9, ...), also with an extension, plus names that
/// Win32 silently rewrites (trailing dot/space) or that address an alternate data stream (`:`).
fn is_windows_reserved(name: &str) -> bool {
    if name.ends_with(['.', ' ']) || name.contains(':') {
        return true;
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

/// Removes `.`/`..` without touching the filesystem (`..` at the root is dropped).
pub fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// `fs::canonicalize` without the Windows verbatim prefix (`\\?\C:\x` -> `C:\x`, `\\?\UNC\s\h` ->
/// `\\s\h`), which git and most tools reject. Use it for every path handed to a child process.
pub fn canonicalize(p: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    dunce::canonicalize(p)
}

/// Realpath of `p`, or of its nearest existing ancestor plus the missing tail.
/// `None` when a dangling symlink is on the path.
pub fn realpath_lenient(p: &Path) -> Option<PathBuf> {
    let mut tail = Vec::new();
    let mut cur = p;
    loop {
        match canonicalize(cur) {
            Ok(mut real) => {
                real.extend(tail.iter().rev());
                return Some(real);
            }
            Err(_) if fs::symlink_metadata(cur).is_ok() => return None,
            Err(_) => {
                tail.push(cur.file_name()?);
                cur = cur.parent()?;
            }
        }
    }
}

/// Component-wise containment (`/a/bc` is not inside `/a/b`), folding case per policy.
/// Purely lexical: callers pass realpaths when symlinks matter.
pub fn contains(root: &Path, p: &Path, policy: PathPolicy) -> bool {
    let key = |c: Component| -> String {
        let s = match c {
            // `\\?\C:` (from canonicalize) equals `C:`.
            Component::Prefix(pr) => match pr.kind() {
                Prefix::VerbatimDisk(d) | Prefix::Disk(d) => format!("{}:", d as char),
                _ => c.as_os_str().to_string_lossy().into_owned(),
            },
            c => c.as_os_str().to_string_lossy().into_owned(),
        };
        if policy.case_insensitive {
            s.to_lowercase()
        } else {
            s
        }
    };
    let mut pc = p.components();
    root.components()
        .all(|r| pc.next().is_some_and(|x| key(x) == key(r)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _dir: tempfile::TempDir,
        base: PathBuf, // canonical temp dir
        ws: WorkspaceContext,
    }

    /// base/p (workspace), base/pp (sibling with same prefix), base/outside, base/extra
    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let base = canonicalize(dir.path()).unwrap();
        for d in ["p/sub", "p/subdir", "pp", "outside", "extra"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        fs::write(base.join("p/sub/f.txt"), "x").unwrap();
        fs::write(base.join("outside/secret"), "x").unwrap();
        let ws = WorkspaceContext::open(
            "w",
            &base.join("p"),
            &base.join("p/sub"),
            &[base.join("extra")],
            base.join("art"),
            "pg",
        )
        .unwrap();
        Fx {
            _dir: dir,
            base,
            ws,
        }
    }

    fn kind(r: Result<PathBuf, ResolveError>) -> &'static str {
        match r {
            Ok(_) => "ok",
            Err(ResolveError::Outside(_)) => "outside",
            Err(ResolveError::OutsideViaLink(_)) => "link",
            Err(ResolveError::Network(_)) => "network",
            Err(ResolveError::Io(_)) => "io",
        }
    }

    #[test]
    fn open_sets_the_stable_keys_from_the_root() {
        let f = fx();
        let root = f.base.join("p");
        assert_eq!(
            f.ws.workspace_key,
            crate::workspace_key(&root, f.ws.path_policy)
        );
        assert_eq!(f.ws.workspace_key.len(), 24);
        assert_eq!(f.ws.workspace_label, "p");
        assert_eq!(f.ws.repo_key, f.ws.workspace_key, "not a git repository");
    }

    #[test]
    fn table() {
        let f = fx();
        let b = f.base.to_str().unwrap();
        let cases: Vec<(String, &str)> = vec![
            ("".into(), "ok"),
            ("f.txt".into(), "ok"),
            ("./f.txt".into(), "ok"),
            ("new/not/yet/there.txt".into(), "ok"),
            ("a/../f.txt".into(), "ok"),
            (format!("{b}/p/sub/f.txt"), "ok"),
            (format!("{b}/extra/x"), "ok"),
            ("..".into(), "outside"), // cwd is p/sub; parent p is not a root
            ("../../outside/secret".into(), "outside"),
            ("../subdir".into(), "outside"), // sibling prefix: p/sub vs p/subdir
            (format!("{b}/p/subdir/x"), "outside"),
            (format!("{b}/pp/x"), "outside"),
            (format!("{b}/p/sub/../../outside"), "outside"),
            ("/".into(), "outside"),
            ("~/definitely-not-in-workspace".into(), "ok"), // literal, no expansion
        ];
        for (input, want) in cases {
            assert_eq!(kind(f.ws.resolve(&input)), want, "input {input:?}");
        }
    }

    #[test]
    fn returns_normalised_absolute_path() {
        let f = fx();
        assert_eq!(
            f.ws.resolve("a/../f.txt").unwrap(),
            f.base.join("p/sub/f.txt")
        );
    }

    #[test]
    fn tilde_is_literal() {
        let f = fx();
        let cwd = f.base.join("p/sub");
        assert_eq!(f.ws.resolve("~/x").unwrap(), cwd.join("~").join("x"));
        assert_eq!(f.ws.resolve("~").unwrap(), cwd.join("~"));
    }

    #[test]
    fn lexical_normalize_cases() {
        let n = |s: &str| lexical_normalize(Path::new(s));
        assert_eq!(n("/a/./b/../c"), PathBuf::from("/a/c"));
        assert_eq!(n("/.."), PathBuf::from("/"));
        assert_eq!(n("a/../../b"), PathBuf::from("b"));
    }

    #[test]
    fn realpath_lenient_cases() {
        let f = fx();
        let real = realpath_lenient(&f.base.join("p/new/x")).unwrap();
        assert_eq!(real, f.base.join("p/new/x"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.base.join("nowhere"), f.base.join("dangling")).unwrap();
            assert!(realpath_lenient(&f.base.join("dangling/x")).is_none());
        }
    }

    #[test]
    fn contains_is_component_wise() {
        let p = PathPolicy {
            case_insensitive: false,
            windows_separators: false,
        };
        let c = |r: &str, x: &str| contains(Path::new(r), Path::new(x), p);
        assert!(c("/a/b", "/a/b"));
        assert!(c("/a/b", "/a/b/c"));
        assert!(!c("/a/b", "/a/bc"));
        assert!(!c("/a/b", "/a"));
        assert!(!c("/a/b", "/a/B/c"));
        let ci = PathPolicy {
            case_insensitive: true,
            ..p
        };
        assert!(contains(Path::new("/a/b"), Path::new("/A/B/c"), ci));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_of_the_root_resolves_to_the_canonical_path() {
        use std::os::unix::fs::symlink;
        let f = fx();
        let alias = f.base.join("alias");
        symlink(f.base.join("p"), &alias).unwrap();
        symlink(f.base.join("outside"), f.base.join("p/sub/esc")).unwrap();
        let want = f.base.join("p/sub/f.txt");
        assert_eq!(
            f.ws.resolve(alias.join("sub/f.txt").to_str().unwrap())
                .unwrap(),
            want
        );
        assert_eq!(
            f.ws.resolve(alias.join("sub/new/deep.txt").to_str().unwrap())
                .unwrap(),
            f.base.join("p/sub/new/deep.txt")
        );
        for bad in [
            "sub/esc/secret",
            "sub/esc/new",
            "../outside/secret",
            "../pp",
        ] {
            let r = f.ws.resolve(alias.join(bad).to_str().unwrap());
            assert!(r.is_err(), "{bad}: {r:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tmp_and_private_tmp_are_the_same_workspace() {
        let d = tempfile::tempdir_in("/tmp").unwrap();
        let root = canonicalize(d.path()).unwrap();
        assert!(root.starts_with("/private/tmp"));
        let ws = WorkspaceContext::open("w", &root, &root, &[], root.join("art"), "pg").unwrap();
        let via_tmp = d.path().join("a.txt");
        assert!(via_tmp.starts_with("/tmp"));
        assert_eq!(
            ws.resolve(via_tmp.to_str().unwrap()).unwrap(),
            root.join("a.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_dangling() {
        use std::os::unix::fs::symlink;
        let f = fx();
        let sub = f.base.join("p/sub");
        symlink(f.base.join("outside"), sub.join("link")).unwrap();
        symlink(f.base.join("outside/secret"), sub.join("flink")).unwrap();
        symlink(f.base.join("nowhere"), sub.join("dangling")).unwrap();
        symlink(sub.join("f.txt"), sub.join("inner")).unwrap();
        for (input, want) in [
            ("link", "link"),
            ("link/secret", "link"),
            ("link/new-file", "link"),
            ("flink", "link"),
            ("dangling", "link"),
            ("dangling/x", "link"),
            ("inner", "ok"),
        ] {
            assert_eq!(kind(f.ws.resolve(input)), want, "input {input:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn backslash_is_ordinary_on_posix() {
        let f = fx();
        // `..\..\outside` is ONE filename inside cwd, not a traversal.
        let p = f.ws.resolve("..\\..\\outside").unwrap();
        assert_eq!(p, f.base.join("p/sub").join("..\\..\\outside"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn case_folded() {
        let f = fx();
        let upper = f.base.join("P/SUB/F.TXT");
        assert!(f.ws.resolve(upper.to_str().unwrap()).is_ok());
        assert_eq!(
            kind(f.ws.resolve(f.base.join("P/SUBDIR").to_str().unwrap())),
            "outside"
        );
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn case_sensitive_on_linux() {
        let f = fx();
        assert_eq!(
            kind(f.ws.resolve(f.base.join("P/sub/x").to_str().unwrap())),
            "outside"
        );
    }

    #[cfg(windows)]
    #[test]
    fn unc_is_network() {
        let f = fx();
        assert_eq!(kind(f.ws.resolve(r"\\host\share\x")), "network");
    }

    #[test]
    fn windows_policy_rejects_reserved_names_and_ads() {
        let mut f = fx();
        f.ws.path_policy.windows_separators = true;
        for bad in [
            "CON",
            "nul",
            "a/COM1",
            "lpt9.txt",
            "aux.tar.gz",
            "x/PRN",
            "name.",
            "name ",
            "f.txt:ads",
            "d/f.txt::$DATA",
        ] {
            assert_eq!(kind(f.ws.resolve(bad)), "io", "input {bad:?}");
        }
        for ok in ["f.txt", "console", "com10", "a.b/c", "COMx"] {
            assert_eq!(kind(f.ws.resolve(ok)), "ok", "input {ok:?}");
        }
    }

    #[cfg(unix)] // colon and device names are invalid in a real Windows filesystem
    #[test]
    fn posix_policy_allows_colon_and_device_names() {
        let f = fx();
        assert_eq!(kind(f.ws.resolve("CON")), "ok");
        assert_eq!(kind(f.ws.resolve("a:b")), "ok");
    }

    #[test]
    fn open_rejects_cwd_outside_root() {
        let f = fx();
        let e = WorkspaceContext::open(
            "w",
            &f.base.join("p"),
            &f.base.join("outside"),
            &[],
            f.base.clone(),
            "pg",
        );
        assert!(matches!(e, Err(ResolveError::Outside(_))));
    }
}
