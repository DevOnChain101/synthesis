//! Check planning (verification.md §4.2). Reads files only; never runs anything.

use crate::types::*;
use std::fs;
use std::path::Path;

fn chk(kind: CheckKind, cmd: &str, cwd: &Path) -> Check {
    Check {
        id: String::new(),
        kind,
        command: Some(cmd.into()),
        cwd: cwd.into(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
        declared_by: Declarer::Harness,
        criteria_ids: vec![],
    }
}

/// Project commands discovered in `dir` (shortcut: top level only, no monorepo walk).
pub fn detect_checks(dir: &Path) -> Vec<Check> {
    let mut out = vec![];
    let has = |f: &str| dir.join(f).is_file();
    let read = |f: &str| fs::read_to_string(dir.join(f)).unwrap_or_default();

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&read("package.json")) {
        let pm = if has("pnpm-lock.yaml") {
            "pnpm"
        } else if has("yarn.lock") {
            "yarn"
        } else {
            "npm"
        };
        for (script, kind) in [
            ("typecheck", CheckKind::Typecheck),
            ("lint", CheckKind::Lint),
            ("build", CheckKind::Build),
            ("test", CheckKind::Unit),
        ] {
            let Some(body) = v["scripts"][script].as_str() else {
                continue;
            };
            if script == "test" && body.contains("no test specified") {
                continue; // npm init placeholder
            }
            out.push(chk(kind, &format!("{pm} run {script}"), dir));
        }
    }
    if has("Cargo.toml") {
        out.push(chk(CheckKind::Syntax, "cargo fmt --check", dir));
        out.push(chk(
            CheckKind::Lint,
            "cargo clippy --all-targets -- -D warnings",
            dir,
        ));
        out.push(chk(CheckKind::Unit, "cargo test", dir));
    }
    let py = if cfg!(windows) { "python" } else { "python3" };
    let pytest_cfg = has("pytest.ini") || read("pyproject.toml").contains("pytest");
    let has_py_tests = [dir.to_path_buf(), dir.join("tests"), dir.join("test")]
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("test") && n.ends_with(".py")
        });
    if pytest_cfg {
        out.push(chk(CheckKind::Unit, &format!("{py} -m pytest"), dir));
    } else if has_py_tests {
        out.push(chk(
            CheckKind::Unit,
            &format!("{py} -m unittest discover"),
            dir,
        ));
    }
    if read("Makefile")
        .lines()
        .any(|l| l.starts_with("test:") || l.starts_with("test :"))
    {
        out.push(chk(CheckKind::Unit, "make test", dir));
    }
    if has("go.mod") {
        out.push(chk(CheckKind::Unit, "go test ./...", dir));
    }
    out
}

/// A recognised verification invocation: one plain command (no shell syntax) that only builds,
/// lints or tests. The harness may run it unasked when the harness itself inferred it
/// (docs/specs/verification.md §4.3). Shortcut: token match on the leading words, so `make` takes
/// no variable overrides and flags that run other programs are refused; widen on demand.
pub fn is_verification_command(cmd: &str) -> bool {
    const PLAIN: &str = " _.-/=:,@+";
    if cmd.is_empty()
        || !cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || PLAIN.contains(c))
    {
        return false;
    }
    let t: Vec<&str> = cmd.split_whitespace().collect();
    if t.iter().any(|a| {
        ["--fix", "--config", "-exec", "-toolexec", "-vettool"]
            .iter()
            .any(|bad| a == bad || a.starts_with(&format!("{bad}=")))
    }) {
        return false;
    }
    match t.as_slice() {
        ["cargo", "fmt", rest @ ..] => rest.contains(&"--check"),
        ["cargo", "clippy" | "test" | "check", ..] => true,
        [
            "npm" | "pnpm" | "yarn",
            "run",
            "test" | "lint" | "typecheck" | "build",
            ..,
        ] => true,
        ["node", "--test", ..] | ["pytest", ..] => true,
        ["python" | "python3", "-m", "pytest" | "unittest", ..] => true,
        ["go", "test" | "vet", ..] => true,
        ["make", "test" | "lint" | "check"] => true,
        ["tsc", rest @ ..] => rest.contains(&"--noEmit"),
        _ => false,
    }
}

/// Whether `changed` (workspace-relative paths) can affect what the inferred check `cmd` exercises.
/// A change set of only unrelated files (a `.txt`, a README) makes the check noise.
fn relevant(cmd: &str, changed: &[String]) -> bool {
    let exts: &[&str] = match cmd.split_whitespace().next() {
        Some("cargo") => &["rs", "toml", "lock"],
        Some("npm" | "pnpm" | "yarn") => &[
            "js", "jsx", "ts", "tsx", "mjs", "cjs", "json", "vue", "svelte",
        ],
        Some("python" | "python3") => &["py", "toml", "ini", "cfg"],
        Some("go") => &["go", "mod", "sum"],
        Some("make") => {
            return changed
                .iter()
                .any(|f| !f.ends_with(".md") && !f.ends_with(".txt"));
        }
        _ => return true,
    };
    changed.iter().any(|f| {
        Path::new(f)
            .extension()
            .is_some_and(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
    })
}

/// Plan order: user-declared criterion checks, model-registered checks, inferred checks.
/// Inferred checks duplicating an earlier command are dropped; ids `k1..` are assigned to
/// checks that have none. With a non-empty `changed`, inferred checks unrelated to it are dropped.
pub fn plan_checks(
    criteria: &[Criterion],
    model_checks: &[Check],
    dir: &Path,
    changed: &[String],
) -> Vec<Check> {
    plan_checks_from(criteria, model_checks, dir, detect_checks(dir), changed)
}

/// [`plan_checks`] with the inferred checks given (frozen at task start) instead of re-read.
pub(crate) fn plan_checks_from(
    criteria: &[Criterion],
    model_checks: &[Check],
    dir: &Path,
    inferred: Vec<Check>,
    changed: &[String],
) -> Vec<Check> {
    let mut out: Vec<Check> = vec![];
    for c in criteria
        .iter()
        .filter(|c| c.source == CriterionSource::User)
    {
        if let Some(cmd) = &c.check {
            let mut k = chk(CheckKind::Custom, cmd, dir);
            k.declared_by = Declarer::User;
            k.criteria_ids = vec![c.id.clone()];
            out.push(k);
        }
    }
    out.extend(model_checks.iter().cloned().map(|mut k| {
        k.declared_by = Declarer::Model;
        k
    }));
    for k in inferred {
        let rel = changed.is_empty() || k.command.as_deref().is_none_or(|c| relevant(c, changed));
        if rel && !out.iter().any(|o| o.command == k.command) {
            out.push(k);
        }
    }
    for (i, k) in out.iter_mut().enumerate() {
        if k.id.is_empty() {
            k.id = format!("k{}", i + 1);
        }
    }
    out
}

/// Files that define what the checks run (workspace-relative, `/`-separated): a change to one
/// during a task is a weakening candidate. Shortcut: top level only, like `detect_checks`.
pub(crate) fn is_definition_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    matches!(name, "Cargo.toml" | "package.json")
        || (!rel.contains('/')
            && (matches!(
                name,
                "Makefile"
                    | "makefile"
                    | "GNUmakefile"
                    | "pyproject.toml"
                    | "pytest.ini"
                    | "tox.ini"
                    | "setup.cfg"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | "go.mod"
                    | "tsconfig.json"
            ) || ["jest.config.", "vitest.config.", ".mocharc"]
                .iter()
                .any(|p| name.starts_with(p))))
        || rel == ".cargo/config.toml"
        || (rel.starts_with(".github/workflows/") && !rel[18..].contains('/'))
}

/// What of a definition file counts: `package.json` only by its `scripts`, `Cargo.toml` without
/// its dependency tables (adding a dependency does not change what `cargo test` runs).
pub(crate) fn definition_view(rel: &str, text: &str) -> String {
    match rel.rsplit('/').next().unwrap_or(rel) {
        "package.json" => serde_json::from_str::<serde_json::Value>(text)
            .map_or_else(|_| text.into(), |v| v["scripts"].to_string()),
        "Cargo.toml" => {
            let mut skip = false;
            let mut out = String::new();
            for l in text.lines() {
                if l.trim_start().starts_with('[') {
                    skip = l.contains("dependencies");
                }
                if !skip {
                    out += l;
                    out.push('\n');
                }
            }
            out
        }
        _ => text.into(),
    }
}

/// Source of the script a check command runs (`npm run x` -> the `scripts.x` body, `make test`
/// -> its recipe), read from the files as they are now.
fn script_body(dir: &Path, cmd: &str) -> Option<String> {
    let t: Vec<&str> = cmd.split_whitespace().collect();
    match t.as_slice() {
        ["npm" | "pnpm" | "yarn", "run", name, ..] => {
            let v: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
            Some(v["scripts"][name].as_str()?.into())
        }
        ["make", "test"] => {
            let mk = fs::read_to_string(dir.join("Makefile")).ok()?;
            let mut lines = mk
                .lines()
                .skip_while(|l| !(l.starts_with("test:") || l.starts_with("test :")))
                .skip(1);
            let recipe: Vec<&str> = lines
                .by_ref()
                .take_while(|l| l.starts_with('\t'))
                .map(|l| l.trim().trim_start_matches(['@', '-']))
                .collect();
            Some(recipe.join(";"))
        }
        _ => None,
    }
}

/// A command that cannot test anything: `true`, `:`, `exit 0`, `echo ...` (every `&&`/`;` part).
pub(crate) fn is_noop_command(cmd: &str) -> bool {
    let mut parts = cmd
        .split("&&")
        .flat_map(|p| p.split(';'))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .peekable();
    parts.peek().is_some()
        && parts.all(|p| {
            let w: Vec<&str> = p.split_whitespace().collect();
            matches!(
                w.first().copied(),
                Some(":" | "true" | "/bin/true" | "/usr/bin/true" | "echo" | "printf")
            ) || matches!(w.as_slice(), ["exit"] | ["exit", "0"])
        })
}

/// Whether the check does nothing now: its command, or the script behind it, is a no-op.
pub(crate) fn is_noop_check(dir: &Path, cmd: &str) -> bool {
    is_noop_command(cmd) || script_body(dir, cmd).is_some_and(|b| is_noop_command(&b))
}

/// Existing files named in the scripts behind `checks` (`node scripts/t.js` -> `scripts/t.js`).
pub(crate) fn referenced_scripts(dir: &Path, checks: &[Check]) -> Vec<String> {
    let mut out: Vec<String> = checks
        .iter()
        .filter_map(|c| script_body(dir, c.command.as_deref()?))
        .flat_map(|b| {
            b.split_whitespace()
                .map(|t| {
                    t.trim_matches(['"', '\''])
                        .trim_start_matches("./")
                        .to_string()
                })
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.starts_with('-') && !t.starts_with('/') && dir.join(t).is_file())
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_without_running_and_maps_criteria() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::write(
            p.join("package.json"),
            r#"{"scripts":{"test":"node --test","lint":"x","build":"y","start":"z"}}"#,
        )
        .unwrap();
        fs::write(p.join("Cargo.toml"), "").unwrap();
        fs::write(p.join("go.mod"), "").unwrap();
        fs::write(p.join("Makefile"), "all:\n\ttrue\ntest:\n\ttrue\n").unwrap();
        fs::write(p.join("test_a.py"), "").unwrap();
        let cmds: Vec<_> = detect_checks(p)
            .into_iter()
            .map(|c| c.command.unwrap())
            .collect();
        for want in [
            "npm run test",
            "npm run lint",
            "npm run build",
            "cargo test",
            "cargo fmt --check",
            "go test ./...",
            "make test",
        ] {
            assert!(cmds.iter().any(|c| c == want), "{want} in {cmds:?}");
        }
        assert!(cmds.iter().any(|c| c.ends_with("-m unittest discover")));
        assert!(!cmds.iter().any(|c| c.contains("start")));

        let crit = [Criterion::user("c1", "works", Some("cargo test"))];
        let plan = plan_checks(&crit, &[], p, &[]);
        assert_eq!(plan[0].declared_by, Declarer::User);
        assert_eq!(plan[0].criteria_ids, ["c1"]);
        assert_eq!(
            plan.iter()
                .filter(|c| c.command.as_deref() == Some("cargo test"))
                .count(),
            1
        );
    }

    #[test]
    fn recognises_only_plain_verification_commands() {
        for ok in [
            "cargo fmt --check",
            "cargo clippy --all-targets -- -D warnings",
            "cargo test -p x -- --nocapture",
            "cargo check",
            "npm run test",
            "pnpm run lint",
            "yarn run typecheck",
            "npm run build",
            "node --test",
            "pytest -q tests/",
            "python3 -m pytest",
            "python -m unittest discover",
            "go test ./...",
            "go vet ./...",
            "make test",
            "tsc --noEmit",
        ] {
            assert!(is_verification_command(ok), "{ok}");
        }
        for bad in [
            "",
            "cargo fmt",
            "cargo fix",
            "cargo test && rm -rf x",
            "cargo test; id",
            "cargo test $(id)",
            "cargo test > out",
            "cargo clippy --fix",
            "cargo test --config build.rustc-wrapper=sh",
            "go test -exec sh ./...",
            "npm run deploy",
            "npm test",
            "make test CC=sh",
            "make install",
            "tsc",
            "python -m http.server",
            "curl x",
        ] {
            assert!(!is_verification_command(bad), "{bad}");
        }
    }

    #[test]
    fn every_detected_check_is_recognised() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Cargo.toml"), "").unwrap();
        fs::write(d.path().join("go.mod"), "").unwrap();
        fs::write(d.path().join("Makefile"), "test:\n").unwrap();
        fs::write(d.path().join("pytest.ini"), "").unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"typecheck":"x","lint":"x","build":"x","test":"x"}}"#,
        )
        .unwrap();
        let all = detect_checks(d.path());
        assert!(all.len() >= 9);
        for c in all {
            let cmd = c.command.unwrap();
            assert!(is_verification_command(&cmd), "{cmd}");
        }
    }

    #[test]
    fn unrelated_changes_drop_inferred_checks() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Cargo.toml"), "").unwrap();
        let cmds = |ch: &[&str]| {
            let ch: Vec<String> = ch.iter().map(|s| s.to_string()).collect();
            plan_checks(&[], &[], d.path(), &ch).len()
        };
        assert_eq!(cmds(&["hello.txt"]), 0);
        assert_eq!(cmds(&["src/lib.rs"]), 3);
        assert_eq!(cmds(&[]), 3);
    }

    #[test]
    fn npm_placeholder_test_is_skipped() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert!(detect_checks(d.path()).is_empty());
    }

    #[test]
    fn noop_and_definition_helpers() {
        for n in [
            "true",
            ":",
            "exit 0",
            "echo ok",
            "echo a && true",
            "/bin/true",
        ] {
            assert!(is_noop_command(n), "{n}");
        }
        for r in [
            "",
            "node --test",
            "echo x && exit 1",
            "cargo test",
            "true && node t.js",
        ] {
            assert!(!is_noop_command(r), "{r}");
        }
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"test":"true","lint":"node scripts/l.js"}}"#,
        )
        .unwrap();
        fs::create_dir(d.path().join("scripts")).unwrap();
        fs::write(d.path().join("scripts/l.js"), "").unwrap();
        assert!(is_noop_check(d.path(), "npm run test"));
        assert!(!is_noop_check(d.path(), "npm run lint"));
        let checks = detect_checks(d.path());
        assert_eq!(referenced_scripts(d.path(), &checks), ["scripts/l.js"]);
        assert!(is_definition_file("package.json") && is_definition_file("crates/a/Cargo.toml"));
        assert!(is_definition_file(".github/workflows/ci.yml") && !is_definition_file("src/a.rs"));
        assert_eq!(
            definition_view(
                "Cargo.toml",
                "[package]\nname=\"a\"\n[dependencies]\nx=1\n[[test]]\nname=\"t\"\n"
            ),
            "[package]\nname=\"a\"\n[[test]]\nname=\"t\"\n"
        );
    }
}
