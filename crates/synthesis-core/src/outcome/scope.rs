//! Which files a commit may change under an envelope (ADR-020 section 3): the pure check the
//! independent runner applies to the list of changed files, so a write the permission rules could
//! not see (a shell redirect, a program the worker built) still fails verification.

use super::contract::{Capability, CapabilityMode, Envelope};

#[derive(Clone, Copy, PartialEq)]
enum Tok {
    Lit(u8),
    One,
    Star,
    Deep,
}

/// `*` within a segment, `**` across segments, `?` one character (the glob language of envelope
/// path patterns). Iterative (a set of reachable text positions per token, no backtracking),
/// consecutive wildcards collapsed. `None` when the work would exceed the budget: callers choose
/// the safe reading.
pub fn glob_matches(pat: &[u8], text: &[u8]) -> Option<bool> {
    const MAX_STEPS: usize = 2_000_000;
    let mut toks: Vec<Tok> = vec![];
    let mut i = 0;
    while i < pat.len() {
        let t = match pat[i] {
            b'*' => {
                let mut j = i;
                while j < pat.len() && pat[j] == b'*' {
                    j += 1;
                }
                let t = if j - i >= 2 { Tok::Deep } else { Tok::Star };
                i = j;
                match (toks.last().copied(), t) {
                    (Some(Tok::Deep), _) | (Some(Tok::Star), Tok::Star) => continue,
                    (Some(Tok::Star), Tok::Deep) => {
                        toks.pop();
                    }
                    _ => {}
                }
                t
            }
            b'?' => {
                i += 1;
                Tok::One
            }
            c => {
                i += 1;
                Tok::Lit(c)
            }
        };
        toks.push(t);
    }
    if toks.len().saturating_mul(text.len() + 1) > MAX_STEPS {
        return None;
    }
    let n = text.len();
    let mut cur = vec![false; n + 1];
    cur[0] = true;
    for t in toks {
        let mut next = vec![false; n + 1];
        match t {
            Tok::Lit(c) => (0..n).for_each(|k| next[k + 1] = cur[k] && text[k] == c),
            Tok::One => (0..n).for_each(|k| next[k + 1] = cur[k] && text[k] != b'/'),
            Tok::Star | Tok::Deep => {
                let mut open = false;
                for k in 0..=n {
                    open = open || cur[k];
                    next[k] = open;
                    if k < n && t == Tok::Star && text[k] == b'/' {
                        open = false;
                    }
                }
            }
        }
        cur = next;
    }
    Some(cur[n])
}

fn matches_pattern(pattern: &str, path: &str) -> Option<bool> {
    glob_matches(pattern.as_bytes(), path.as_bytes())
}

/// The changed files (worktree-relative, `/` separators) that the envelope does not allow. A file
/// is allowed only if the envelope allows `repo.write`, some `allowed_paths` pattern matches it and
/// no `denied_paths` entry matches it (a denied entry also denies what is under it). A path the
/// matcher cannot decide, or that is not a plain relative path, is NOT allowed. Case is folded:
/// on a case-insensitive file system `Src/x` is `src/x`.
pub fn outside_envelope(changed: &[String], env: &Envelope) -> Vec<String> {
    let may_write = env.mode(Capability::RepoWrite) == CapabilityMode::Allow;
    let lower = |s: &str| s.to_lowercase();
    changed
        .iter()
        .filter(|f| {
            let p = lower(f);
            let plain = !p.is_empty()
                && !p.starts_with('/')
                && !p.contains('\\')
                && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..");
            let denied = env.denied_paths.iter().any(|d| {
                let d = lower(d);
                matches_pattern(&d, &p).unwrap_or(true)
                    || matches_pattern(&format!("{d}/**"), &p).unwrap_or(true)
            });
            let allowed = env
                .allowed_paths
                .iter()
                .any(|a| matches_pattern(&lower(a), &p).unwrap_or(false));
            !(may_write && plain && allowed && !denied)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::contract::Budgets;

    fn env(allowed: &[&str], denied: &[&str], write: bool) -> Envelope {
        let mut capabilities = std::collections::BTreeMap::new();
        if write {
            capabilities.insert(Capability::RepoWrite, CapabilityMode::Allow);
        }
        Envelope {
            allowed_paths: allowed.iter().map(|s| (*s).into()).collect(),
            denied_paths: denied.iter().map(|s| (*s).into()).collect(),
            capabilities,
            budgets: Budgets::new(1.0),
            quality_gates: vec![],
        }
    }

    fn out(changed: &[&str], e: &Envelope) -> Vec<String> {
        outside_envelope(
            &changed.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            e,
        )
    }

    #[test]
    fn globs_follow_the_envelope_language() {
        assert_eq!(glob_matches(b"src/**", b"src/a/b.rs"), Some(true));
        assert_eq!(glob_matches(b"src/*", b"src/a/b.rs"), Some(false));
        assert_eq!(glob_matches(b"src/*.rs", b"src/b.rs"), Some(true));
        assert_eq!(glob_matches(b"src/***/k", b"src/a/b/k"), Some(true));
        assert_eq!(glob_matches(b"a?c", b"a/c"), Some(false));
        // too costly to decide: no answer
        assert_eq!(glob_matches(&b"*a".repeat(3000), &b"a".repeat(3000)), None);
    }

    #[test]
    fn a_change_outside_the_envelope_is_reported() {
        let e = env(&["src/**", "tests/*.rs"], &["src/secret"], true);
        assert!(out(&["src/a.rs", "src/deep/b.rs", "tests/t.rs"], &e).is_empty());
        let bad = out(
            &[
                "README.md",
                "src/secret/k",
                "src/secret",
                "tests/sub/t.rs",
                "Src/../x",
                "SRC/A.RS",
            ],
            &e,
        );
        assert_eq!(
            bad,
            [
                "README.md",
                "src/secret/k",
                "src/secret",
                "tests/sub/t.rs",
                "Src/../x"
            ],
            "case is folded: SRC/A.RS is src/a.rs"
        );
        // not allowed to write at all: every change is outside
        assert_eq!(
            out(&["src/a.rs"], &env(&["src/**"], &[], false)),
            ["src/a.rs"]
        );
        // an empty allow list allows nothing
        assert_eq!(out(&["a"], &env(&[], &[], true)), ["a"]);
        // odd spellings are not plain relative paths
        for odd in ["/etc/x", "src\\a.rs", "src//a.rs", "./src/a.rs", ""] {
            assert_eq!(out(&[odd], &e), [odd], "{odd:?}");
        }
    }
}
