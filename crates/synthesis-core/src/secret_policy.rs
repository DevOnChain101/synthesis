//! The secret-path rule as one value: the built-in name rules of [`crate::secrets`] plus the patterns of
//! `synt.secrets.deny`. Detection is by PATH NAME, never by content. The patterns can only ADD denials:
//! nothing here can make a built-in secret path readable, and an invalid pattern is dropped with a
//! diagnostic while the defaults stay.
//!
//! Pattern syntax (gitignore-like, deliberately small):
//! - `/` and `\` are the same; case is ignored on Windows and macOS. A trailing `/` is allowed.
//! - `*` any run within one segment, `**` any run across segments, `?` one character. No `[]`/`{}`.
//! - No `/` in the pattern (`.env.local`, `*.pfx`): matches a file or folder of that name at any depth.
//! - Other relative patterns (`secrets/**`, `config/prod.json`, `**/keys/*.json`) are anchored at the workspace
//!   root; a leading `**/` also matches zero folders.
//! - `~/x` (the home folder) and absolute paths (`/etc/x`, `C:/x`) match that place.
//! - A pattern that matches a folder covers everything below it.
//! - `!` (negation), `#`, empty patterns and `..` are invalid: a pattern can never allow a path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use unicode_normalization::UnicodeNormalization;

use crate::secrets;
use crate::{PathPolicy, canonicalize, lexical_normalize};

/// Longest pattern and longest list taken; the rest is dropped with a diagnostic (the built-ins stay).
pub const MAX_PATTERN: usize = 256;
pub const MAX_PATTERNS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// No `/`: any path segment.
    Name,
    /// Anchored at the workspace root.
    Rel,
    Abs,
}

/// One pattern segment: `**` (zero or more path segments) or a name with `*`/`?` (`*` never crosses a `/`).
#[derive(Debug, Clone)]
enum Seg {
    Deep,
    Lit(Vec<char>),
}

#[derive(Debug, Clone)]
struct Pat {
    raw: String,
    kind: Kind,
    segs: Vec<Seg>,
    /// Original-case text (`~` expanded, `//` collapsed) for building a concrete path.
    shown: String,
}

/// What the OS sandbox can be given for the patterns (see [`DenyList::sandbox_paths`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SandboxCoverage {
    pub paths: Vec<PathBuf>,
    /// One sentence per pattern the OS sandbox cannot (fully) enforce; the permission layer still does.
    pub permission_only: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DenyList {
    pats: Vec<Pat>,
    ci: bool,
}

/// Matching form of a text: `/` separators, NFC and lower case where the file system folds (macOS, Windows).
fn fold(s: &str, ci: bool) -> String {
    let n = s.replace('\\', "/");
    let n = match n.strip_prefix("//?/") {
        Some(r) => r.to_string(),
        None => n,
    };
    if ci {
        n.nfc().collect::<String>().to_lowercase()
    } else {
        n
    }
}

fn wild(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// `*` (any run) and `?` (one character) against one path segment.
fn seg_fit(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) && p[pi] != '*' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Does the pattern cover `t` or a folder above it (a pattern that names a folder covers what is below)?
fn covers(segs: &[Seg], t: &[Vec<char>]) -> bool {
    let (np, nt) = (segs.len(), t.len());
    let mut ok = vec![vec![false; nt + 1]; np + 1];
    ok[np].iter_mut().for_each(|x| *x = true);
    for pi in (0..np).rev() {
        for ti in (0..=nt).rev() {
            ok[pi][ti] = match &segs[pi] {
                Seg::Deep => ok[pi + 1][ti] || (ti < nt && ok[pi][ti + 1]),
                Seg::Lit(l) => ti < nt && seg_fit(l, &t[ti]) && ok[pi + 1][ti + 1],
            };
        }
    }
    ok[0][0]
}

fn chars_of(s: &str) -> Vec<Vec<char>> {
    s.split('/')
        .filter(|x| !x.is_empty())
        .map(|x| x.chars().collect())
        .collect()
}

impl DenyList {
    /// Parses `patterns`; every invalid one is dropped and named in the returned diagnostics. `home` expands
    /// `~/`; `root` (the workspace) only sharpens the diagnostic for an absolute pattern.
    pub fn new(
        patterns: &[String],
        home: Option<&Path>,
        root: Option<&Path>,
        policy: PathPolicy,
    ) -> (DenyList, Vec<String>) {
        let ci = policy.case_insensitive;
        let (mut pats, mut diag) = (vec![], vec![]);
        if patterns.len() > MAX_PATTERNS {
            diag.push(format!(
                "synt.secrets.deny: {} patterns given, only the first {MAX_PATTERNS} are used; the built-in secret rules still apply",
                patterns.len()
            ));
        }
        for raw in patterns.iter().take(MAX_PATTERNS) {
            match Pat::parse(raw, home, ci) {
                Ok(p) => {
                    let lit = raw.trim().replace('\\', "/");
                    if p.kind == Kind::Abs && lit.starts_with('/') {
                        let under = |b: Option<&Path>| {
                            b.is_some_and(|b| {
                                fold(&p.shown, ci).starts_with(fold(&b.to_string_lossy(), ci).trim_end_matches('/'))
                            })
                        };
                        if !under(root) && !under(home) {
                            diag.push(format!(
                                "synt.secrets.deny: pattern {raw:?} is an absolute path (use ./x for a path inside the workspace)"
                            ));
                        }
                    }
                    pats.push(p);
                }
                Err(e) => diag.push(format!(
                    "synt.secrets.deny: pattern {:?} ignored ({e}); the built-in secret rules still apply",
                    raw.chars().take(80).collect::<String>()
                )),
            }
        }
        (DenyList { pats, ci }, diag)
    }

    pub fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.pats.iter().map(|p| p.raw.as_str())
    }

    /// Does a pattern deny `path` (this spelling only; see [`SecretPolicy`] for links)? `root` is the
    /// workspace root; a path outside it is matched by name patterns and absolute patterns only.
    pub fn matches(&self, path: &Path, root: &Path) -> bool {
        if self.pats.is_empty() {
            return false;
        }
        // `a/./b`, `a//b` and `x/../a` are the same place as `a/b`
        let (path, root) = (lexical_normalize(path), lexical_normalize(root));
        let full = fold(&path.to_string_lossy(), self.ci);
        let base = fold(&root.to_string_lossy(), self.ci);
        let base = base.trim_end_matches('/');
        let rel = (!base.is_empty())
            .then(|| full.strip_prefix(base).filter(|r| r.starts_with('/')))
            .flatten()
            // a relative `path` is already workspace-relative
            .or_else(|| (!path.is_absolute() && !full.starts_with('/')).then_some(full.as_str()));
        let (full_t, rel_t) = (chars_of(&full), rel.map(chars_of));
        self.pats.iter().any(|p| match p.kind {
            Kind::Name => {
                let Seg::Lit(l) = &p.segs[0] else {
                    return false;
                };
                rel_t
                    .as_ref()
                    .unwrap_or(&full_t)
                    .iter()
                    .any(|s| seg_fit(l, s))
            }
            Kind::Rel => rel_t.as_ref().is_some_and(|r| covers(&p.segs, r)),
            Kind::Abs => covers(&p.segs, &full_t),
        })
    }

    /// The patterns that name concrete places become sandbox read-deny paths: a literal path, a literal
    /// name (at the workspace root only) or a folder (`dir/**`). Everything else (a wildcard in the
    /// middle, `**/*.pfx`) is reported in `permission_only`.
    pub fn sandbox_paths(&self, root: &Path) -> SandboxCoverage {
        let mut out = SandboxCoverage::default();
        for p in &self.pats {
            let s = p.shown.as_str();
            let dir = s
                .strip_suffix("/**")
                .or_else(|| s.strip_suffix("/*"))
                .unwrap_or(s);
            if wild(dir) || dir.starts_with("**") {
                out.permission_only.push(format!(
                    "pattern {:?} is enforced by the permission layer only, not by the OS sandbox on this platform",
                    p.raw
                ));
                continue;
            }
            let path = match p.kind {
                Kind::Abs => PathBuf::from(dir),
                _ => root.join(dir),
            };
            if !path.is_absolute() {
                out.permission_only.push(format!(
                    "pattern {:?} is enforced by the permission layer only, not by the OS sandbox on this platform",
                    p.raw
                ));
                continue;
            }
            match p.kind {
                Kind::Abs | Kind::Rel => out.paths.push(path),
                Kind::Name => {
                    out.paths.push(path);
                    out.permission_only.push(format!(
                        "pattern {:?}: the OS sandbox covers only <workspace>/{dir}; copies in sub-folders are enforced by the permission layer only",
                        p.raw
                    ));
                }
            }
        }
        out.paths.sort();
        out.paths.dedup();
        out
    }
}

impl Pat {
    fn parse(raw: &str, home: Option<&Path>, ci: bool) -> Result<Pat, String> {
        let t = raw.trim();
        if t.is_empty() {
            return Err("empty".into());
        }
        if t.chars().count() > MAX_PATTERN {
            return Err(format!("longer than {MAX_PATTERN} characters"));
        }
        if t.starts_with(['!', '#']) {
            return Err("a pattern can only deny: no negation or comments".into());
        }
        if t.contains(['[', ']', '{', '}', '\0']) {
            return Err("[], {} and NUL are not supported".into());
        }
        let mut s = t.replace('\\', "/");
        let lead = s.starts_with("//") as usize;
        while s[lead..].contains("//") {
            s = format!("{}{}", &s[..lead], s[lead..].replace("//", "/"));
        }
        let s = s.trim_end_matches('/');
        if s.is_empty() || s.split('/').any(|c| c == "..") {
            return Err("empty or contains ..".into());
        }
        let drive = s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && s.as_bytes().get(1) == Some(&b':');
        let (kind, shown) = if let Some(rest) = s.strip_prefix("~/") {
            let h = home.ok_or("no home folder to expand ~")?;
            let h = h.to_string_lossy().replace('\\', "/");
            (Kind::Abs, format!("{}/{rest}", h.trim_end_matches('/')))
        } else if s == "~" {
            return Err("use ~/<path>".into());
        } else if s.starts_with('/') || drive {
            (Kind::Abs, s.to_string())
        } else {
            let r = s.strip_prefix("./").unwrap_or(s);
            let flat = r.strip_prefix("**/").unwrap_or(r);
            let k = if flat.contains('/') {
                Kind::Rel
            } else {
                Kind::Name
            };
            (
                k,
                if k == Kind::Name {
                    flat.to_string()
                } else {
                    r.to_string()
                },
            )
        };
        let segs = fold(&shown, ci)
            .split('/')
            .filter(|x| !x.is_empty())
            .map(|x| {
                if x == "**" {
                    Seg::Deep
                } else {
                    Seg::Lit(x.chars().collect())
                }
            })
            .collect::<Vec<_>>();
        if segs.is_empty() {
            return Err("empty".into());
        }
        Ok(Pat {
            raw: raw.to_string(),
            kind,
            segs,
            shown,
        })
    }
}

/// The built-in name rules plus the user's deny list. Cheap to clone; the default is the built-ins alone.
/// Every place that withholds a file from a model, a picker, a diff or a commit asks this value.
#[derive(Debug, Clone, Default)]
pub struct SecretPolicy {
    deny: Arc<DenyList>,
    /// Last workspace root and its real path (a stat per check otherwise).
    canon_root: Arc<Mutex<Option<(PathBuf, PathBuf)>>>,
}

impl SecretPolicy {
    pub fn new(deny: DenyList) -> Self {
        Self {
            deny: Arc::new(deny),
            canon_root: Arc::default(),
        }
    }

    pub fn deny_list(&self) -> &DenyList {
        &self.deny
    }

    /// Paths for `SandboxPolicy::deny_paths`: never one that contains the workspace (that would hide it all).
    pub fn sandbox_deny_paths(&self, root: &Path) -> Vec<PathBuf> {
        let mut v = self.deny.sandbox_paths(root).paths;
        v.retain(|d| !root.starts_with(d));
        v
    }

    fn real_root(&self, root: &Path) -> PathBuf {
        let mut c = self.canon_root.lock().unwrap_or_else(|e| e.into_inner());
        match &*c {
            Some((r, real)) if r == root => real.clone(),
            _ => {
                let real = canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                *c = Some((root.to_path_buf(), real.clone()));
                real
            }
        }
    }

    /// Only the user's patterns (no built-in names): this spelling and its real path. `root` is the WORKSPACE
    /// root the relative patterns are anchored at, whatever folder the caller's own paths are relative to.
    pub fn denies(&self, path: &Path, root: &Path) -> bool {
        !self.deny.is_empty()
            && (self.deny.matches(path, root)
                || canonicalize(if path.is_relative() {
                    root.join(path)
                } else {
                    path.to_path_buf()
                })
                .is_ok_and(|r| self.deny.matches(&r, &self.real_root(root))))
    }

    /// [`secrets::is_secret_path`] (a name) or a pattern, on this spelling and its real path.
    pub fn is_secret_path(&self, path: &Path, root: &Path) -> bool {
        secrets::is_secret_path(path) || self.denies(path, root)
    }

    /// [`secrets::is_secret_shaped_in`] or a pattern.
    pub fn is_secret_shaped_in(&self, path: &Path, root: &Path) -> bool {
        secrets::is_secret_shaped_in(path, root) || self.denies(path, root)
    }

    /// [`secrets::is_secret_target_in`] or a pattern (the path and its real path).
    pub fn is_secret_target_in(&self, path: &Path, root: &Path) -> bool {
        secrets::is_secret_target_in(path, root) || self.denies(path, root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: PathPolicy = PathPolicy {
        case_insensitive: true,
        windows_separators: false,
    };
    const CS: PathPolicy = PathPolicy {
        case_insensitive: false,
        windows_separators: false,
    };

    fn dl(p: &[&str], pol: PathPolicy) -> (DenyList, Vec<String>) {
        let v: Vec<String> = p.iter().map(|s| s.to_string()).collect();
        DenyList::new(&v, Some(Path::new("/h/u")), Some(Path::new("/w/proj")), pol)
    }
    fn hit(d: &DenyList, p: &str) -> bool {
        d.matches(Path::new(p), Path::new("/w/proj"))
    }

    #[test]
    fn syntax() {
        let (d, diag) = dl(
            &[
                ".env.local",
                "secrets/**",
                "**/*.pfx",
                "~/vault/**",
                "/opt/k",
                "a/**/b.json",
            ],
            CS,
        );
        // only the absolute `/opt/k` (outside workspace and home) is flagged
        assert!(
            diag.len() == 1 && diag[0].contains("/opt/k") && diag[0].contains("./x"),
            "{diag:?}"
        );
        for p in [
            "/w/proj/.env.local",
            "/w/proj/x/y/.env.local",
            "/w/proj/secrets/a/b",
            "/w/proj/c.pfx",
            "/w/proj/x/c.pfx",
            "/h/u/vault/k",
            "/opt/k",
            "/opt/k/x",
            "/w/proj/a/b.json",
            "/w/proj/a/z/b.json",
            "/elsewhere/c.pfx",
        ] {
            assert!(hit(&d, p), "{p}");
        }
        for p in [
            "/w/proj/src/x.rs",
            "/w/proj/x/secrets/a",
            "/w/proj/.env.example",
            "/h/u/other",
            "/w/proj/sub/a/b.json",
        ] {
            assert!(!hit(&d, p), "{p}");
        }
        // a relative path is workspace-relative
        assert!(d.matches(Path::new("secrets/x"), Path::new("/w/proj")));
        // other spellings of the same place
        for p in [
            "/w/proj/./secrets/x",
            "/w/proj//secrets/x",
            "/w/proj/a/../secrets/x",
            "secrets/../secrets/x",
        ] {
            assert!(hit(&d, p), "{p}");
        }
    }

    #[test]
    fn several_double_star_groups_match_zero_or_more_folders_and_do_not_over_match() {
        let t = |pats: &[&str], yes: &[&str], no: &[&str]| {
            let (d, diag) = dl(pats, CS);
            assert!(diag.is_empty(), "{diag:?}");
            for p in yes {
                assert!(
                    hit(&d, &format!("/w/proj/{p}")),
                    "{pats:?} should match {p}"
                );
            }
            for p in no {
                assert!(
                    !hit(&d, &format!("/w/proj/{p}")),
                    "{pats:?} must not match {p}"
                );
            }
        };
        t(
            &["**/keys/**/*.json"],
            &[
                "keys/b.json",
                "keys/a/b.json",
                "x/keys/b.json",
                "x/y/keys/a/b/c.json",
            ],
            &["keys/b.txt", "key/b.json", "x/b.json", "keys"],
        );
        t(
            &["src/**/keys/**/*.json"],
            &[
                "src/keys/a/b.json",
                "src/keys/b.json",
                "src/x/keys/y/z.json",
            ],
            &[
                "keys/b.json",
                "other/keys/b.json",
                "src/keys/b.txt",
                "src/x/b.json",
            ],
        );
        t(
            &["a/**/b/**/c"],
            &["a/b/x/c", "a/b/c", "a/q/b/c", "a/q/r/b/s/t/c/d"],
            &["a/b/x/d", "b/c", "z/a/b/c", "a/c"],
        );
        t(&["a/**/**/c"], &["a/c", "a/x/y/c"], &["a/x"]);
    }

    #[test]
    fn question_mark_is_one_character_and_unicode_is_normalised_where_the_file_system_folds() {
        let (d, _) = dl(&["caf?.key", "\u{e9}.cert"], CS);
        assert!(
            hit(&d, "/w/proj/caf\u{e9}.key"),
            "? is one character, not one byte"
        );
        assert!(!hit(&d, "/w/proj/cafee.key") && !hit(&d, "/w/proj/caf.key"));
        // NFD name (e + combining acute) against an NFC pattern: the same file on macOS
        let (ci, _) = dl(&["\u{e9}.cert"], CI);
        assert!(hit(&ci, "/w/proj/e\u{301}.cert"));
        assert!(
            !hit(&d, "/w/proj/e\u{301}.cert"),
            "case-sensitive systems keep the bytes"
        );
    }

    #[test]
    fn caps_and_double_slashes() {
        let long = "a".repeat(MAX_PATTERN + 1);
        let many: Vec<String> = (0..MAX_PATTERNS + 3).map(|i| format!("p{i}")).collect();
        let (d, diag) = DenyList::new(&many, None, None, CS);
        assert_eq!(d.patterns().count(), MAX_PATTERNS);
        assert!(diag.iter().any(|x| x.contains("only the first")));
        let (d, diag) = DenyList::new(&[long, "secrets//deep///**".into()], None, None, CS);
        assert_eq!(diag.len(), 1, "{diag:?}");
        assert!(d.matches(Path::new("/w/proj/secrets/deep/x"), Path::new("/w/proj")));
    }

    #[test]
    fn case_is_folded_only_where_the_policy_says() {
        let (ci, _) = dl(&["Secrets/**", "*.PFX"], CI);
        assert!(hit(&ci, "/w/proj/SECRETS/a") && hit(&ci, "/w/proj/a.pfx"));
        let (cs, _) = dl(&["Secrets/**", "*.PFX"], CS);
        assert!(
            !hit(&cs, "/w/proj/SECRETS/a")
                && !hit(&cs, "/w/proj/a.pfx")
                && hit(&cs, "/w/proj/Secrets/a")
        );
        let (bs, _) = dl(&[r"secrets\**"], CS);
        assert!(bs.matches(Path::new(r"secrets\a"), Path::new("/w/proj")));
    }

    #[test]
    fn invalid_patterns_are_dropped_with_a_diagnostic() {
        let (d, diag) = dl(&["!src/x", "", "#c", "a/../b", "[ab]", "~", "ok.key2"], CS);
        assert_eq!(diag.len(), 6, "{diag:?}");
        assert_eq!(d.patterns().collect::<Vec<_>>(), ["ok.key2"]);
        let (d, diag) = DenyList::new(&["~/x".into()], None, None, CS);
        assert!(d.is_empty() && diag.len() == 1);
    }

    #[test]
    fn a_pattern_never_allows_a_built_in_secret() {
        let (d, _) = dl(&["!.env", "src/**"], CS);
        let p = SecretPolicy::new(d);
        assert!(p.is_secret_path(Path::new("/w/proj/.env"), Path::new("/w/proj")));
        assert!(p.is_secret_shaped_in(Path::new("/w/proj/.ssh/x"), Path::new("/w/proj")));
        assert!(!p.is_secret_path(Path::new("/w/proj/.env.example"), Path::new("/w/proj")));
        assert!(p.is_secret_shaped_in(Path::new("/w/proj/src/a.rs"), Path::new("/w/proj")));
    }

    #[test]
    fn a_link_to_a_denied_place_is_denied() {
        #[cfg(unix)]
        {
            let t = tempfile::tempdir().unwrap();
            let root = canonicalize(t.path()).unwrap();
            std::fs::create_dir_all(root.join("vault")).unwrap();
            std::fs::write(root.join("vault/k"), "x").unwrap();
            std::os::unix::fs::symlink(root.join("vault/k"), root.join("innocent.txt")).unwrap();
            let (d, _) = DenyList::new(&["vault/**".into()], None, None, CS);
            let p = SecretPolicy::new(d);
            assert!(p.is_secret_target_in(&root.join("innocent.txt"), &root));
            assert!(!p.is_secret_target_in(&root.join("vault.txt"), &root));
        }
    }

    #[test]
    #[cfg(unix)] // the sample paths are POSIX absolute
    fn sandbox_paths_and_the_rest() {
        let (d, _) = dl(
            &[
                "secrets/**",
                "~/vault/**",
                "/opt/k",
                ".env.local",
                "**/*.pfx",
                "a/*/b",
            ],
            CS,
        );
        let c = d.sandbox_paths(Path::new("/w/proj"));
        for p in [
            "/w/proj/secrets",
            "/h/u/vault",
            "/opt/k",
            "/w/proj/.env.local",
        ] {
            assert!(c.paths.contains(&PathBuf::from(p)), "{p}: {:?}", c.paths);
        }
        assert_eq!(c.paths.len(), 4);
        assert_eq!(c.permission_only.len(), 3, "{:?}", c.permission_only);
        assert!(
            c.permission_only
                .iter()
                .any(|l| l.contains("**/*.pfx") && l.contains("permission layer only"))
        );
    }
}
