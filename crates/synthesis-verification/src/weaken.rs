//! Detects agent edits that weaken tests (verification.md §3a): a pass is worth less when the
//! agent also changed what the check asserts. Input is the line diff of each edited file.

use crate::types::{PREREQ_GUARD_REMOVED, TEST_FILES_MODIFIED};
use regex::Regex;
use std::sync::LazyLock;

/// Added and removed lines of one file edit, as in the Edit/Write tool diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileEdit {
    pub path: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

static TEST_PATH: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(^|/)(tests?|__tests__|specs?|e2e|fixtures?|testdata|__snapshots__|golden|snapshots|expected)/|\.(test|spec|snap)\.\w+$|\.snap$|(^|/)test_[^/]*$|_tests?\.\w+$|_spec\.rb$|(Test|Tests|IT)\.java$|(^|/)conftest[^/]*\.py$",
    )
});
static SKIP: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\.(skip|only|todo)\b|\b(xit|xdescribe|xtest|fit|fdescribe)\(|\bskip\s*[:=(]|@(pytest\.mark\.(skip|skipif|xfail)|unittest\.(skip|expectedFailure)|Disabled|Ignore)|pytest\.(skip|xfail)\(|#\[ignore|\bt\.Skip|\bxfail\b|\bpending\(",
    )
});
static TEST_CASE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"#\[(tokio::)?test\b|\b(it|test)(\.\w+)?\(|\bdef test_|\bfunc Test\w*\(|@Test\b")
});
static ENV_SHORTCUT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(process\.env\.\w+|process\.env\[[^\]]+\]|os\.environ(\[[^\]]+\]|\.setdefault)|env::set_var|os\.Setenv|putenv)\s*(\?\?=|\|\|=|=[^=]|\()|os\.environ\.setdefault",
    )
});
static ASSERT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"\bassert\w*[!(\s.]|\bexpect\(|\.should\b|\bt\.(Error|Fatal)f?\(|\.to(Be|Equal|Throw|Match|Have)\w*\(|\bself\.assert",
    )
});

// `if (!url) throw new Error('DB_URL is not set')`, `raise RuntimeError("API_KEY missing")`
static GUARD: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(throw|raise|panic!|exit\(|Error\().*(not set|missing|required|not defined|undefined|not configured)",
    )
});
static ENV_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"\b[A-Z][A-Z0-9]*_[A-Z0-9_]+\b|(?i)\benv"));

/// Whether `path` (workspace-relative) is a test or fixture file.
pub(crate) fn is_test_path(path: &str) -> bool {
    TEST_PATH.is_match(&path.replace('\\', "/"))
}

/// Fixture, snapshot and golden files: any change to an existing one moves the expected values.
pub(crate) fn is_golden_path(path: &str) -> bool {
    static GOLDEN: LazyLock<Regex> = LazyLock::new(|| {
        re(r"(?i)(^|/)(fixtures?|testdata|__snapshots__|golden|snapshots|expected)/|\.snap$")
    });
    GOLDEN.is_match(&path.replace('\\', "/"))
}

pub(crate) fn count_test_cases(lines: &[String]) -> usize {
    count(lines, &TEST_CASE)
}

/// A one-line literal table row (`{"a", 1, 2},`): changing one changes an expected value.
pub(crate) fn count_table_rows(lines: &[String]) -> usize {
    static ROW: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*[\{\[\(].*,.*[\}\]\)],?\s*$"));
    count(lines, &ROW)
}

pub(crate) fn count_assertions(lines: &[String]) -> usize {
    count(lines, &ASSERT)
}

fn count(lines: &[String], r: &Regex) -> usize {
    lines.iter().filter(|l| r.is_match(l)).count()
}

/// One limitation line, `test files modified: ...`, when an edit to a test or fixture file adds
/// a skip/only/ignore/xfail/todo marker, adds an environment-variable short-circuit, or removes
/// more assertions than it adds. `None` when no edit weakens a check.
pub fn weakened_tests(edits: &[FileEdit]) -> Option<String> {
    let mut found = vec![];
    for e in edits {
        let is_test = is_test_path(&e.path);
        let mut why = vec![];
        // `#[ignore]` is unambiguous in any Rust file (unit tests live next to the code).
        if (is_test && count(&e.added, &SKIP) > 0)
            || e.path.ends_with(".rs") && e.added.iter().any(|l| l.contains("#[ignore"))
        {
            why.push("added skip/only/ignore/xfail/todo marker");
        }
        if is_test && count(&e.added, &ENV_SHORTCUT) > 0 {
            why.push("sets or short-circuits an environment variable");
        }
        if is_test && count(&e.removed, &ASSERT) > count(&e.added, &ASSERT) {
            why.push("removed assertions");
        }
        if is_test && count(&e.removed, &TEST_CASE) > count(&e.added, &TEST_CASE) {
            why.push("removed test cases");
        }
        if !why.is_empty() {
            found.push(format!("{} ({})", e.path, why.join("; ")));
        }
    }
    (!found.is_empty()).then(|| format!("{TEST_FILES_MODIFIED}: {}", found.join(", ")))
}

/// `prerequisite guard removed: ...` when an edit to any file deletes a check that fails loudly
/// on a missing credential or env var (the usual way to "fix" it is to default it).
pub fn removed_prerequisite_guards(edits: &[FileEdit]) -> Option<String> {
    let found: Vec<&str> = edits
        .iter()
        .filter(|e| {
            e.removed
                .iter()
                .any(|l| GUARD.is_match(l) && ENV_NAME.is_match(l))
                && !e.added.iter().any(|l| GUARD.is_match(l))
        })
        .map(|e| e.path.as_str())
        .collect();
    (!found.is_empty()).then(|| format!("{PREREQ_GUARD_REMOVED}: {}", found.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &str, added: &[&str], removed: &[&str]) -> FileEdit {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
        FileEdit {
            path: path.into(),
            added: v(added),
            removed: v(removed),
        }
    }

    #[test]
    fn flags_a_removed_missing_env_guard() {
        let g = |added: &[&str], removed: &[&str]| {
            removed_prerequisite_guards(&[edit("src/db.js", added, removed)])
        };
        let guard = "  if (!url) throw new Error('DB_URL is not set');";
        assert!(
            g(
                &["const url = process.env.DB_URL || 'sqlite::memory:';"],
                &[guard]
            )
            .is_some()
        );
        assert!(g(&[], &["raise RuntimeError('API_KEY missing')"]).is_some());
        // the guard is kept (moved), or the removed line is unrelated
        assert!(
            g(
                &["  if (!url) throw new Error('DB_URL is not set');"],
                &[guard]
            )
            .is_none()
        );
        assert!(g(&[], &["throw new Error('invalid argument')"]).is_none());
    }

    #[test]
    fn flags_weakening_edits_only() {
        let w = |e: FileEdit| weakened_tests(&[e]);
        assert!(
            w(edit(
                "test/db.test.js",
                &["test('x', { skip: !process.env.DB_URL }, () => {"],
                &["test('x', () => {"]
            ))
            .is_some()
        );
        assert!(
            w(edit(
                "test/db.test.js",
                &["process.env.DB_URL ??= 'postgres://x'"],
                &[]
            ))
            .unwrap()
            .contains("environment")
        );
        assert!(w(edit("tests/test_a.py", &["@pytest.mark.skip"], &[])).is_some());
        assert!(w(edit("src/lib.rs", &["#[ignore]"], &[])).is_some());
        assert!(
            w(edit("tests/a.rs", &[], &["assert_eq!(a, b);"]))
                .unwrap()
                .contains("assertions")
        );
        // replacing an assertion with another, a new test, and non-test files are fine
        assert!(
            w(edit(
                "tests/a.rs",
                &["assert_eq!(a, c);"],
                &["assert_eq!(a, b);"]
            ))
            .is_none()
        );
        assert!(
            w(edit(
                "tests/a.rs",
                &["#[test]", "fn t() { assert!(x); }"],
                &[]
            ))
            .is_none()
        );
        assert!(
            w(edit(
                "src/db.js",
                &["opts.skip = true", "process.env.X = 'y'"],
                &["assert(x)"]
            ))
            .is_none()
        );
    }
}
