//! Test-runner summary parsing (verification.md §2). Parsing only ever downgrades a pass:
//! a check that skipped or ignored tests, or ran none, proves less than its exit code says.

use crate::types::TestCounts;
use regex::Regex;
use std::sync::LazyLock;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

static CARGO: LazyLock<Regex> =
    LazyLock::new(|| re(r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored"));
static NEXTEST: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^.*Summary \[.*\] +\d+ tests? run:.*$"));
static NODE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^(?:# |ℹ )(pass|fail|skipped|todo|cancelled) (\d+)\s*$"));
// jest "Tests:       1 skipped, 2 passed, 3 total" and vitest "Tests  2 passed | 1 skipped (3)"
static JEST: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*Tests:?\s+\d.*$"));
static PYTEST: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?m)^[= ]*((?:\d+ (?:passed|failed|skipped|xfailed|xpassed|errors?|deselected|warnings?)(?:, )?)+) in [\d.]+s",
    )
});
static GO: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*--- (PASS|FAIL|SKIP):"));
static MAVEN: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^\[\w+\] Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: (\d+)\s*$")
});
static GRADLE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(\d+) tests completed(?:, (\d+) failed)?(?:, (\d+) skipped)?"));
static NUM_WORD: LazyLock<Regex> =
    LazyLock::new(|| re(r"(\d+) (passed|failed|skipped|todo|xfailed|xpassed|errors?)"));
static SKIP_LINE: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^.*(?:# SKIP|# TODO|SKIPPED|--- SKIP:|\.\.\. ignored|○ skipped|^\s*﹣).*$")
});

fn n(s: &str) -> u64 {
    s.parse().unwrap_or(0)
}

fn words(line: &str, c: &mut TestCounts) {
    for m in NUM_WORD.captures_iter(line) {
        let v = n(&m[1]);
        match &m[2] {
            "passed" | "xpassed" => c.passed += v,
            "skipped" | "xfailed" => c.skipped += v,
            "todo" => c.todo += v,
            _ => c.failed += v,
        }
    }
}

static UNITTEST: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?m)^Ran (\d+) tests? in [\d.]+s\s*\n\s*\n(OK|FAILED)[^\n]*"));
/// Counts from whichever known runner summaries `out` contains (summed); `None` when none is
/// recognised (for example `go test` without `-v`, or a custom script).
pub fn parse_counts(out: &str) -> Option<TestCounts> {
    let mut c = TestCounts::default();
    let mut seen = false;
    for m in CARGO.captures_iter(out) {
        seen = true;
        c.passed += n(&m[1]);
        c.failed += n(&m[2]);
        c.skipped += n(&m[3]);
    }
    for m in NEXTEST.find_iter(out) {
        seen = true;
        words(m.as_str(), &mut c);
    }
    for m in NODE.captures_iter(out) {
        seen = true;
        let v = n(&m[2]);
        match &m[1] {
            "pass" => c.passed += v,
            "skipped" => c.skipped += v,
            "todo" => c.todo += v,
            _ => c.failed += v,
        }
    }
    for m in JEST.find_iter(out) {
        seen = true;
        words(m.as_str(), &mut c);
    }
    for m in PYTEST.captures_iter(out) {
        seen = true;
        words(&m[1], &mut c);
    }
    for m in GO.captures_iter(out) {
        seen = true;
        match &m[1] {
            "PASS" => c.passed += 1,
            "SKIP" => c.skipped += 1,
            _ => c.failed += 1,
        }
    }
    for m in UNITTEST.captures_iter(out) {
        seen = true;
        let run = n(&m[1]);
        let skipped = m[0]
            .split("skipped=")
            .nth(1)
            .map_or(0, |t| n(t.trim_end_matches(')').trim()));
        if &m[2] == "OK" {
            c.passed += run.saturating_sub(skipped);
            c.skipped += skipped;
        } else {
            c.failed += run;
        }
    }
    if let Some(m) = MAVEN.captures_iter(out).last() {
        seen = true;
        let (run, f, e, s) = (n(&m[1]), n(&m[2]), n(&m[3]), n(&m[4]));
        c.passed += run.saturating_sub(f + e + s);
        c.failed += f + e;
        c.skipped += s;
    }
    for m in GRADLE.captures_iter(out) {
        seen = true;
        let (all, f, s) = (
            n(&m[1]),
            m.get(2).map_or(0, |x| n(x.as_str())),
            m.get(3).map_or(0, |x| n(x.as_str())),
        );
        c.passed += all.saturating_sub(f + s);
        c.failed += f;
        c.skipped += s;
    }
    if !seen {
        return None;
    }
    c.skipped_names = SKIP_LINE
        .find_iter(out)
        .take(5)
        .map(|l| l.as_str().trim().chars().take(120).collect())
        .collect();
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(out: &str) -> (u64, u64, u64, u64) {
        let c = parse_counts(out).expect("recognised");
        (c.passed, c.failed, c.skipped, c.todo)
    }

    #[test]
    fn runners() {
        assert_eq!(
            c(
                "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured\ntest result: ok. 2 passed; 1 failed; 0 ignored;"
            ),
            (5, 1, 1, 0)
        );
        assert_eq!(
            c("     Summary [   0.01s] 4 tests run: 3 passed (1 slow), 1 skipped"),
            (3, 0, 1, 0)
        );
        assert_eq!(
            c("# tests 3\n# pass 1\n# fail 0\n# cancelled 0\n# skipped 1\n# todo 1\n"),
            (1, 0, 1, 1)
        );
        assert_eq!(
            c("ℹ pass 2\nℹ fail 0\nℹ skipped 0\nℹ todo 0\n"),
            (2, 0, 0, 0)
        );
        assert_eq!(c("Tests:       1 skipped, 2 passed, 3 total"), (2, 0, 1, 0));
        assert_eq!(
            c("      Tests  2 passed | 1 skipped | 1 todo (4)"),
            (2, 0, 1, 1)
        );
        assert_eq!(
            c("===== 1 failed, 2 passed, 1 skipped in 0.12s ====="),
            (2, 1, 1, 0)
        );
        assert_eq!(c("2 passed, 1 xfailed in 0.01s"), (2, 0, 1, 0));
        assert_eq!(
            c("--- PASS: TestA (0.00s)\n--- SKIP: TestB (0.00s)\n"),
            (1, 0, 1, 0)
        );
        assert_eq!(
            c(
                "[INFO] Tests run: 1, Failures: 0, Errors: 0, Skipped: 0, Time elapsed: 1 s - in A\n[INFO] Tests run: 5, Failures: 0, Errors: 1, Skipped: 1\n"
            ),
            (3, 1, 1, 0)
        );
        assert_eq!(c("3 tests completed, 1 failed, 1 skipped"), (1, 1, 1, 0));
        assert!(parse_counts("ok  \texample.com/x\t0.01s\n").is_none());
    }

    #[test]
    fn names_the_skipped_tests() {
        let k = parse_counts("﹣ needs db (0.1ms) # SKIP\n✔ other (1ms)\nℹ pass 1\nℹ skipped 1\n")
            .unwrap();
        assert_eq!(k.skipped_names, ["﹣ needs db (0.1ms) # SKIP"]);
    }
}
