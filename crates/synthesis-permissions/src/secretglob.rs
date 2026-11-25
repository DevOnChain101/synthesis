//! Wildcard operands of a Bash reader (`cat secrets/*`, `head certs/*.pfx`) judged against the user's
//! `synt.secrets.deny` patterns. No shell runs: the operand is expanded by listing folders, with a hard bound.

use std::path::{Component, Path, PathBuf};

use synthesis_core::secret_policy::SecretPolicy;

/// Most directory entries looked at and most matches kept for one operand.
const MAX_VISITED: usize = 5000;
const MAX_MATCHES: usize = 1000;

pub(crate) enum Verdict {
    /// Nothing the user denies.
    Clear,
    /// A path or a match of the wildcard is denied.
    Hit,
    /// Could not be decided (a `$`, a backtick, a class `[..]`, `~user`, or too many entries).
    Unknown,
}

/// `*`/`?` of one name against one file name; a leading `*`/`?` does not match a leading dot (shell rule).
fn name_fits(pat: &str, name: &str) -> bool {
    if name.starts_with('.') && !pat.starts_with('.') {
        return false;
    }
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && (p[pi] == '?' || p[pi] == t[ti]) {
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

/// The paths a wildcard operand names, or `None` when that cannot be listed within the bound.
fn expand(base: &Path, operand: &str) -> Option<Vec<PathBuf>> {
    let p = Path::new(operand);
    let mut cur: Vec<PathBuf> = vec![if p.is_absolute() {
        PathBuf::new()
    } else {
        base.to_path_buf()
    }];
    let mut visited = 0;
    for c in p.components() {
        let mut next = vec![];
        match c {
            Component::Normal(n) => {
                let n = n.to_str()?;
                for d in &cur {
                    if !n.contains(['*', '?']) {
                        next.push(d.join(n));
                        continue;
                    }
                    for e in std::fs::read_dir(d).ok()?.flatten() {
                        visited += 1;
                        if visited > MAX_VISITED {
                            return None;
                        }
                        if name_fits(n, &e.file_name().to_string_lossy()) {
                            next.push(e.path());
                        }
                    }
                }
            }
            Component::ParentDir => next = cur.iter().map(|d| d.join("..")).collect(),
            Component::CurDir => next = cur.clone(),
            other => next = cur.iter().map(|d| d.join(other.as_os_str())).collect(),
        }
        if next.len() > MAX_MATCHES {
            return None;
        }
        cur = next;
    }
    Some(cur)
}

/// One word of a command against the user's patterns. `bases` are the folders a relative word may mean.
pub(crate) fn judge(sp: &SecretPolicy, word: &str, bases: &[PathBuf], root: &Path) -> Verdict {
    if sp.deny_list().is_empty() {
        return Verdict::Clear;
    }
    let hit = |p: &Path| sp.is_secret_path(p, root);
    // the text itself, conservatively: `secrets/*` is under `secrets/**`, `*.pfx` is `*.pfx`
    if hit(Path::new(word)) {
        return Verdict::Hit;
    }
    let starts_user = word.starts_with('~') && !word.starts_with("~/") && word != "~";
    if word.contains(['$', '`', '[']) || starts_user {
        return Verdict::Unknown;
    }
    if !word.contains(['*', '?']) {
        return Verdict::Clear; // a literal operand is judged by the existing real-path check
    }
    let mut unknown = false;
    for b in bases {
        match expand(b, word) {
            Some(v) => {
                if v.iter().any(|p| hit(p)) {
                    return Verdict::Hit;
                }
            }
            None => unknown = true,
        }
    }
    if unknown {
        Verdict::Unknown
    } else {
        Verdict::Clear
    }
}
