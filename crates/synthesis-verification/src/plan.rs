//! Check planning (verification.md §4.2). Reads files only; never runs anything.

use crate::types::*;
use regex::Regex;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

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
        out.push(chk(CheckKind::Unit, "go test -v ./...", dir)); // -v: per-test lines the gate can count
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

/// Plan order: user-declared criterion checks, inferred checks, model-registered checks.
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
    for k in inferred {
        let rel = changed.is_empty() || k.command.as_deref().is_none_or(|c| relevant(c, changed));
        if rel && !out.iter().any(|o| o.command == k.command) {
            out.push(k);
        }
    }
    // A model-registered check never replaces a planned one with the same command.
    for mut k in model_checks.iter().cloned() {
        if !out.iter().any(|o| o.command == k.command) {
            k.declared_by = Declarer::Model;
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

/// Files that define what the checks run (workspace-relative, `/`-separated, at any depth): a
/// change to one during a task is a weakening candidate.
pub(crate) fn is_definition_file(rel: &str) -> bool {
    // ONE list for the gate and the permission engine: synthesis_core::definitions
    synthesis_core::definitions::is_check_definition(rel)
}

/// What of a definition file counts: `package.json` only by the keys that decide how tests run,
/// `Cargo.toml` without its dependency tables (adding a dependency does not change what
/// `cargo test` runs).
pub(crate) fn definition_view(rel: &str, text: &str) -> String {
    match rel.rsplit('/').next().unwrap_or(rel) {
        "package.json" => serde_json::from_str::<serde_json::Value>(text).map_or_else(
            |_| text.into(),
            |v| {
                let keep: serde_json::Map<String, serde_json::Value> = [
                    "scripts",
                    "jest",
                    "mocha",
                    "ava",
                    "vitest",
                    "nyc",
                    "c8",
                    "workspaces",
                    "type",
                    "bin",
                    "main",
                    "exports",
                    "packageManager",
                ]
                .iter()
                .filter_map(|k| Some((k.to_string(), v.get(k)?.clone())))
                .collect();
                serde_json::Value::Object(keep).to_string()
            },
        ),
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

/// `&&` / `;` separated parts of a shell line, quotes respected.
fn split_parts(cmd: &str) -> Vec<String> {
    let (mut parts, mut cur, mut quote) = (vec![], String::new(), None::<char>);
    let mut it = cmd.chars().peekable();
    while let Some(c) = it.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, ';') => parts.push(std::mem::take(&mut cur)),
            (None, '&') if it.peek() == Some(&'&') => {
                it.next();
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn make_body(mk: &str, target: &str, depth: u8) -> Option<String> {
    let mut lines = mk.lines().skip_while(|l| {
        l.split_once(':').is_none_or(|(t, rest)| {
            t.trim() != target || rest.starts_with(['=', ':']) || l.starts_with('\t')
        })
    });
    let head = lines.next()?;
    let rest = head.split_once(':')?.1.split('#').next().unwrap_or("");
    let mut out: Vec<String> = vec![];
    if depth > 0 {
        for pre in rest
            .split_whitespace()
            .filter(|p| !p.starts_with('.') && !p.contains(['=', '%', '$']))
        {
            out.extend(make_body(mk, pre, depth - 1));
        }
    }
    out.extend(
        lines
            .take_while(|l| l.starts_with('\t'))
            .map(|l| l.trim().trim_start_matches(['@', '-', '+']).to_string()),
    );
    Some(out.join(";"))
}

/// Source of the script a check command runs (`npm run x` -> pre/main/post bodies of
/// `scripts.x`, `make test` -> its recipe and prerequisites), read from the files as they are now.
/// `None` for anything else, and for a command with shell operators.
fn script_body(dir: &Path, cmd: &str) -> Option<String> {
    if cmd.contains(['|', '>', '<', '`', '$']) {
        return None;
    }
    let t: Vec<&str> = cmd.split_whitespace().collect();
    let npm = |name: &str| -> Option<String> {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
        let get = |n: String| v["scripts"][n].as_str().map(String::from);
        let main = get(name.into())?;
        Some(
            [
                get(format!("pre{name}")),
                Some(main),
                get(format!("post{name}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(";"),
        )
    };
    match t.as_slice() {
        ["npm" | "pnpm" | "yarn", "run", name, ..] => npm(name),
        ["npm" | "pnpm" | "yarn", "test" | "t"] => npm("test"),
        ["yarn", name] => npm(name),
        ["make", target, ..] if !target.starts_with('-') => {
            make_body(&fs::read_to_string(dir.join("Makefile")).ok()?, target, 4)
        }
        _ => None,
    }
}

/// The shell text a check really runs: runner invocations replaced by their script bodies.
fn effective(dir: &Path, cmd: &str, depth: u8) -> String {
    match script_body(dir, cmd).filter(|_| depth > 0) {
        Some(b) => split_parts(&b)
            .iter()
            .map(|p| effective(dir, p, depth - 1))
            .collect::<Vec<_>>()
            .join(";"),
        None => cmd.into(),
    }
}

fn unquote(s: &str) -> &str {
    s.trim().trim_matches(['"', '\''])
}

fn part_is_noop(part: &str) -> bool {
    if part.contains(['|', '>', '<', '`']) || part.contains("$(") {
        return false; // a pipe, redirect or substitution does real work (`echo "$X" | grep -q ok`)
    }
    let w: Vec<&str> = part.split_whitespace().collect();
    let arg = |from: usize| unquote(&w[from.min(w.len())..].join(" ")).to_string();
    match w.as_slice() {
        [] => true,
        [
            ":" | "true" | "/bin/true" | "/usr/bin/true" | "echo" | "printf" | "cd" | "sleep"
            | "pwd",
            ..,
        ] => true,
        ["exit"] | ["exit", "0"] => true,
        ["test" | "[", "1"] | ["test" | "[", "1", "]"] => true,
        ["test" | "[", "-n", x] | ["[", "-n", x, "]"] => !x.contains('$'),
        ["sh" | "bash" | "zsh" | "dash", flag, ..]
            if flag.starts_with('-') && flag.ends_with('c') =>
        {
            is_noop_command(&arg(2))
        }
        ["python" | "python3" | "py", "-c", ..] => matches!(
            arg(2).as_str(),
            "pass"
                | ""
                | "0"
                | "1"
                | "None"
                | "True"
                | "exit(0)"
                | "quit()"
                | "import sys; sys.exit(0)"
        ),
        ["node" | "deno", "-e" | "-p", ..] => matches!(
            arg(2).as_str(),
            "0" | "1" | "" | "true" | "undefined" | "process.exit(0)"
        ),
        _ => false,
    }
}

/// A command that cannot test anything: only `true`, `:`, `exit 0`, `echo`, `cd`, `sleep`,
/// `python -c pass`, `sh -c true` ... in every `&&`/`;` part; or one that cannot fail
/// (`cmd || true`, `cmd; exit 0`, `set +e`, `cmd | true`); or that lists/compiles instead of
/// running (`--list`, `--no-run`, `--collect-only`, `--help`, `--dry-run`).
pub(crate) fn is_noop_command(cmd: &str) -> bool {
    static MASK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"\|\|\s*(true|:|exit\s+0|echo\b)|;\s*(true|:|exit(\s+0)?)\s*$|\bset\s+\+e\b|\|\s*(true|:)\s*$",
        )
        .expect("static regex")
    });
    const LISTING: [&str; 9] = [
        "--list",
        "--list-tests",
        "--listTests",
        "--no-run",
        "--collect-only",
        "--help",
        "--dry-run",
        "--version",
        "-h",
    ];
    !cmd.trim().is_empty()
        && (cmd.split_whitespace().any(|t| LISTING.contains(&t))
            || MASK.is_match(cmd)
            || split_parts(cmd).iter().all(|p| part_is_noop(p)))
}

/// Whether the check proves nothing now: its command, or the scripts behind it (through
/// `npm run`, `yarn`, `pnpm`, `make` prerequisites, pre/post scripts), is a no-op.
pub(crate) fn is_noop_check(dir: &Path, cmd: &str) -> bool {
    is_noop_command(cmd) || is_noop_command(&effective(dir, cmd, 5))
}

/// Existing files named in the scripts behind `checks` (`node scripts/t.js` -> `scripts/t.js`).
pub(crate) fn referenced_scripts(dir: &Path, checks: &[Check]) -> Vec<String> {
    let mut out: Vec<String> = checks
        .iter()
        .map(|c| effective(dir, c.command.as_deref().unwrap_or(""), 5))
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
            "go test -v ./...",
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
            "python3 -c pass",
            "sh -c ':'",
            "bash -c true",
            "node -e 0",
            "test 1",
            "[ 1 ]",
            "sleep 1",
            "cd .",
            "cargo test || true",
            "cargo test; exit 0",
            "set +e; cargo test",
            "cargo test | true",
            "cargo test --no-run",
            "pytest --collect-only",
            "node --test --help",
        ] {
            assert!(is_noop_command(n), "{n}");
        }
        for r in [
            "",
            "node --test",
            "echo x && exit 1",
            "cargo test",
            "true && node t.js",
            "echo \"$X\" | grep -q ok",
            "test -f a.txt",
            "python3 -m pytest",
            "sh -c 'node --test'",
        ] {
            assert!(!is_noop_command(r), "{r}");
        }
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"test":"true","lint":"node scripts/l.js","t2":"npm run test","t3":"npm run lint || true","pretest2":"node x.js","t4":"yarn test"}}"#,
        )
        .unwrap();
        fs::create_dir(d.path().join("scripts")).unwrap();
        fs::write(d.path().join("scripts/l.js"), "").unwrap();
        fs::write(
            d.path().join("Makefile"),
            "test: build\n\t@true\nbuild:\n\t:\nreal:\n\tnode --test\nall: real\n",
        )
        .unwrap();
        assert!(is_noop_check(d.path(), "npm run test"));
        assert!(is_noop_check(d.path(), "npm run t2")); // through another script
        assert!(is_noop_check(d.path(), "npm run t3")); // exit masking behind a script
        assert!(is_noop_check(d.path(), "make test")); // recipe and prerequisite are no-ops
        assert!(!is_noop_check(d.path(), "make all"));
        assert!(!is_noop_check(d.path(), "npm run lint"));
        let checks = detect_checks(d.path());
        assert_eq!(referenced_scripts(d.path(), &checks), ["scripts/l.js"]);
        assert!(is_definition_file("package.json") && is_definition_file("crates/a/Cargo.toml"));
        for f in [
            ".github/workflows/ci.yml",
            "sub/.npmrc",
            "app/vitest.config.ts",
            "clippy.toml",
            "sub/conf.mk",
            "node_modules/.bin/jest",
            ".venv/bin/pytest",
            "build.rs",
            ".cargo/config.toml",
            "x/eslint.config.js",
        ] {
            assert!(is_definition_file(f), "{f}");
        }
        assert!(!is_definition_file("src/a.rs") && !is_definition_file("src/main.js"));
        // the gate and the permission engine read the same list
        for f in synthesis_core::definitions::FILES {
            assert!(is_definition_file(f), "{f}");
            assert!(synthesis_core::definitions::is_definition(f), "{f}");
        }
        for p in synthesis_core::definitions::PREFIXES {
            assert!(is_definition_file(&format!("sub/{p}x.ts")), "{p}");
        }
        // script directories cut the permission trust only: source in `bin/` is not a weakening
        assert!(
            !is_definition_file("bin/run.sh")
                && synthesis_core::definitions::is_definition("bin/run.sh")
        );
        assert_eq!(
            definition_view(
                "Cargo.toml",
                "[package]\nname=\"a\"\n[dependencies]\nx=1\n[[test]]\nname=\"t\"\n"
            ),
            "[package]\nname=\"a\"\n[[test]]\nname=\"t\"\n"
        );
        assert_ne!(
            definition_view("package.json", r#"{"jest":{"testMatch":[]}}"#),
            definition_view("package.json", r#"{"jest":{"testMatch":["x"]}}"#)
        );
    }

    #[test]
    fn a_model_check_never_replaces_a_planned_one() {
        let d = tempfile::tempdir().unwrap();
        let mut planned = chk(CheckKind::Unit, "cargo test", d.path());
        planned.id = "k1".into();
        let mut model = chk(CheckKind::Custom, "cargo test", d.path());
        model.declared_by = Declarer::Model;
        let other = Check {
            command: Some("echo hi".into()),
            ..model.clone()
        };
        let plan = plan_checks_from(&[], &[model, other], d.path(), vec![planned], &[]);
        let by: Vec<_> = plan
            .iter()
            .map(|c| (c.command.clone().unwrap(), c.declared_by, c.kind))
            .collect();
        assert_eq!(by.len(), 2, "{by:?}");
        assert_eq!(
            by[0],
            ("cargo test".into(), Declarer::Harness, CheckKind::Unit)
        );
        assert_eq!(by[1].1, Declarer::Model);
    }
}
