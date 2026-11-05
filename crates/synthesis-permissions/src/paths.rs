//! Path containment, realpath, glob and path-rule matching (Node `permissions.js` 316-428),
//! parameterised by `synthesis_core::PathPolicy` (spec workspace-context.md §4.3).
//!
//! Comparison only: `\` is rewritten to `/` solely when `policy.windows_separators`, never on POSIX.
//! Case is folded solely when `policy.case_insensitive` (windows, macOS).
//!
//!
//! Rule path specs (`Read(...)`, `Edit(...)`): `//abs` = the absolute path `/abs` (Claude Code's
//! convention; `//etc/**` is `/etc/**`), `~/x` = under the home dir (the only place `~` is
//! expanded; tool paths get none), `/x` = relative to cwd, anything else = a glob matched against
//! the full path, the cwd-relative path or the basename. Rules are checked against the lexical
//! path AND its realpath (see `match_path_rule_real`).
//!
//! shortcut: `within` stays a string-key comparison (not `synthesis_core::contains`) because it must
//! honour a Windows `PathPolicy` on any host; the other path helpers come from synthesis-core.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
pub use synthesis_core::lexical_normalize;
use synthesis_core::{PathPolicy, realpath_lenient};

/// `path.resolve(base, p)`: `p` as is when absolute, else under `base`; lexically normalised.
pub fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    lexical_normalize(&if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    })
}

/// Comparison key: `\\?\` prefix stripped and `\` -> `/` on windows, lowercased per policy.
pub fn key(s: &str, policy: PathPolicy) -> String {
    let mut n = s.to_string();
    if policy.windows_separators {
        if let Some(rest) = n.strip_prefix(r"\\?\") {
            n = rest.to_string();
        }
        n = n.replace('\\', "/");
    }
    fold(&n, policy)
}

/// Case folding for case-insensitive policies, NFC first (macOS stores names decomposed).
pub fn fold(s: &str, policy: PathPolicy) -> String {
    if policy.case_insensitive {
        icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
            .normalize(s)
            .to_lowercase()
    } else {
        s.to_string()
    }
}

fn key_p(p: &Path, policy: PathPolicy) -> String {
    key(&p.to_string_lossy(), policy)
}

/// Key without trailing slashes (`/` stays `/`). Port of `nrm`.
pub fn nrm(p: &Path, policy: PathPolicy) -> String {
    let k = key_p(p, policy);
    let t = k.trim_end_matches('/');
    if t.is_empty() {
        "/".to_string()
    } else {
        t.to_string()
    }
}

/// Component-wise containment (`/a/bc` is not inside `/a/b`). Port of `within`.
pub fn within(root: &Path, p: &Path, policy: PathPolicy) -> bool {
    let (r, n) = (nrm(root, policy), nrm(p, policy));
    r == "/" || n == r || n.starts_with(&format!("{r}/"))
}

/// realpath of `p` (or of its nearest existing ancestor plus the missing tail); `None` when a
/// link on the path cannot be resolved (dangling).
pub fn real_of(p: &Path) -> Option<PathBuf> {
    realpath_lenient(p)
}

/// Link count of an existing regular file with more than one hard link (unix only; else `None`).
pub fn hard_links(p: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p)
            .ok()
            .filter(|m| m.is_file() && m.nlink() > 1)
            .map(|m| m.nlink())
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

/// `path.relative(from, to)` over components, `/`-joined.
pub fn relative(from: &Path, to: &Path) -> String {
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); f.len() - common];
    parts.extend(
        t[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// Last path segment of a comparison key / path string.
pub fn basename(s: &str, policy: PathPolicy) -> &str {
    let t = if policy.windows_separators {
        s.trim_end_matches(['/', '\\'])
    } else {
        s.trim_end_matches('/')
    };
    let sep: &[char] = if policy.windows_separators {
        &['/', '\\']
    } else {
        &['/']
    };
    t.rsplit(sep).next().unwrap_or("")
}

/// UNC (`\\host\share`) or `//host/share`; not the `\\?\` and `\\.\` local device prefixes.
/// Applies on every platform, as in Node. Port of `isNetworkPath`.
pub fn is_network_path(s: &str) -> bool {
    let c: Vec<char> = s.chars().take(4).collect();
    let sep = |x: Option<&char>| matches!(x, Some('/' | '\\'));
    sep(c.first()) && sep(c.get(1)) && !(matches!(c.get(2), Some('?' | '.')) && sep(c.get(3)))
}

/// Absolute per Node's `path.isAbsolute` (windows also accepts a leading `/` or `\`).
fn is_abs_str(s: &str, policy: PathPolicy) -> bool {
    Path::new(s).is_absolute() || (policy.windows_separators && s.starts_with(['/', '\\']))
}

/// The literal directory a glob starts in: its leading segments without wildcards.
pub fn glob_static_root(pattern: &str, base: &Path, policy: PathPolicy) -> String {
    static DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z]:$").unwrap());
    let parts: Vec<&str> = pattern.split(['\\', '/']).collect();
    let i = parts
        .iter()
        .position(|s| s.contains(['*', '?', '[', ']', '{', '}', '(', ')', '!']));
    let mut keep: Vec<&str> = i.map_or(parts.clone(), |i| parts[..i].to_vec());
    if keep.len() == 1 && (keep[0].is_empty() || DRIVE.is_match(keep[0])) {
        keep.push("");
    }
    let mut root = keep.join("/");
    // a `..` after the first wildcard climbs out of the static root: use the worst case, the
    // whole pattern normalised as if every wildcard were a directory name
    if i.is_some_and(|i| parts[i..].contains(&"..")) {
        root = parts.join("/");
    }
    if is_abs_str(&root, policy) || is_network_path(&root) {
        root
    } else {
        resolve(base, &root).to_string_lossy().into_owned()
    }
}

/// The directory a `Read(..)` rule spec is anchored in (its literal prefix). A relative spec
/// matches anywhere, so its root is the cwd.
pub fn rule_root(spec: &str, cwd: &Path, home: &Path, policy: PathPolicy) -> PathBuf {
    let root = if let Some(rest) = spec.strip_prefix("//") {
        let drive = rest.as_bytes().get(1) == Some(&b':');
        glob_static_root(
            &if drive {
                rest.to_string()
            } else {
                format!("/{rest}")
            },
            cwd,
            policy,
        )
    } else if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        glob_static_root(rest, home, policy)
    } else if let Some(rest) = spec.strip_prefix('/') {
        glob_static_root(rest, cwd, policy)
    } else {
        return cwd.to_path_buf();
    };
    PathBuf::from(root)
}

/// Glob -> regex: `**` any (a following `/` is consumed), `*` within a segment, `?` one char.
/// Port of `globToRegExp`; `(?s)` so `**` also spans a newline in a file name.
pub fn glob_to_regex(glob: &str) -> Option<Regex> {
    let c: Vec<char> = glob.chars().collect();
    let mut re = String::from("(?s)^");
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' if c.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
                if c.get(i + 1) == Some(&'/') {
                    i += 1;
                }
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            x if ".+^${}()|[]\\".contains(x) => {
                re.push('\\');
                re.push(x);
            }
            x => re.push(x),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// Matches a rule's path spec against `file_path` using Claude Code's conventions:
/// `//abs`, `~/x`, `/x` (cwd-relative), `x` (anywhere). Port of `matchPathRule`.
pub fn match_path_rule(
    spec: &str,
    file_path: &str,
    cwd: &Path,
    home: &Path,
    policy: PathPolicy,
) -> bool {
    if file_path.is_empty() {
        return false;
    }
    let resolved = resolve(cwd, file_path);
    let target = key_p(&resolved, policy);
    let pattern = if let Some(rest) = spec.strip_prefix("//") {
        // `//abs` = `/abs`; `//C:/x` (windows drive) keeps no leading slash
        let drive = rest.as_bytes().get(1) == Some(&b':');
        key(
            &if drive {
                rest.to_string()
            } else {
                format!("/{rest}")
            },
            policy,
        )
    } else if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\")) {
        key_p(&lexical_normalize(&home.join(rest)), policy)
    } else if let Some(rest) = spec.strip_prefix('/') {
        key_p(&resolve(cwd, rest), policy)
    } else {
        // relative pattern anywhere: match the full resolved path, the cwd-relative path or the basename
        let Some(re) = glob_to_regex(&key(spec, policy)) else {
            return false;
        };
        let rel = key(&relative(cwd, &resolved), policy);
        return re.is_match(&target) || re.is_match(&rel) || re.is_match(basename(&target, policy));
    };
    glob_to_regex(&pattern).is_some_and(|re| re.is_match(&target))
}

/// Rule match on the lexical path AND the realpath (a link into a denied dir is denied). `any` =
/// deny/ask semantics (either form matches); otherwise allow semantics (both must match, and an
/// unresolvable link never matches). The real form is evaluated against the real cwd and home.
pub fn match_path_rule_real(
    spec: &str,
    file_path: &str,
    cwd: &Path,
    home: &Path,
    policy: PathPolicy,
    any: bool,
) -> bool {
    let lex = match_path_rule(spec, file_path, cwd, home, policy);
    if file_path.is_empty() {
        return false;
    }
    let real = realpath_lenient(&resolve(cwd, file_path)).map(|r| {
        let rc = realpath_lenient(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let rh = realpath_lenient(home).unwrap_or_else(|| home.to_path_buf());
        match_path_rule(spec, &r.to_string_lossy(), &rc, &rh, policy)
    });
    if any {
        lex || real == Some(true)
    } else {
        lex && real == Some(true)
    }
}

/// Windows-only names that alias or hide another file: NTFS streams (`a:s`), trailing dot/space,
/// 8.3 short names (`PROGRA~1`) and reserved devices (`CON`, `NUL.txt`, `COM1`). Only under a
/// Windows policy; the drive prefix is not a component. Such paths count as protected and outside.
pub fn win_unsafe(path: &str, policy: PathPolicy) -> bool {
    static DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z]:$").unwrap());
    static SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~[0-9]+(\.|$)").unwrap());
    static DEVICE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(con|prn|aux|nul|com[0-9]|lpt[0-9])$").unwrap());
    if !policy.windows_separators {
        return false;
    }
    path.split(['\\', '/']).enumerate().any(|(i, c)| {
        if c.is_empty() || c == "." || c == ".." || c == "?" || (i < 2 && DRIVE.is_match(c)) {
            return false;
        }
        let stem = c.split('.').next().unwrap_or("").trim_end();
        c.contains(':') || c.ends_with(['.', ' ']) || SHORT.is_match(c) || DEVICE.is_match(stem)
    })
}
