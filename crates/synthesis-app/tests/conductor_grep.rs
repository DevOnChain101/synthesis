//! Source-level controls for the conductor (super-agent-s1.md 4.7, 4.8): no conductor module builds the
//! person's click, and no conductor tool can write outside its allowed set. They read the files, so a new
//! call site fails here before a review has to catch it.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn conductor_files() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/conductor");
    let mut out: Vec<(String, String)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(e.path()).unwrap(),
            )
        })
        .collect();
    out.push((
        "synthesis-view/conductor.rs".into(),
        std::fs::read_to_string(root().join("crates/synthesis-view/src/conductor.rs")).unwrap(),
    ));
    out
}

/// The text outside `//` comments. A `#[cfg(test)]` module is NOT cut off: a constructor call there is still a call.
fn code(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !p
                .file_name()
                .is_some_and(|n| n == "target" || n == "node_modules" || n == ".git")
            {
                walk(&p, ext, out);
            }
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
}

fn rel(p: &Path) -> String {
    p.canonicalize()
        .unwrap()
        .strip_prefix(root().canonicalize().unwrap())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// `word` as a whole identifier, wherever it is (aliases, a line break after `::`, a `use`).
fn has_ident(text: &str, word: &str) -> bool {
    let id = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(id)
            && !text[i + word.len()..].chars().next().is_some_and(id)
    })
}

/// Only the card handler of the desktop app builds the person's click (and this crate's conductor tests, through
/// the dev feature `click-test`). No conductor module does, and no other file of `crates/`, `apps/`, `tests/`.
#[test]
fn only_the_card_handler_and_the_conductor_tests_construct_the_persons_click() {
    const ALLOWED: &[&str] = &[
        "crates/synthesis-app/src/conductor/click.rs",
        "crates/synthesis-app/tests/conductor.rs",
        "crates/synthesis-app/tests/conductor_grep.rs",
    ];
    let mut all = vec![];
    for top in ["crates", "apps", "tests"] {
        walk(&root().join(top), "rs", &mut all);
    }
    let mut bad = vec![];
    for f in all {
        let r = rel(&f);
        if ALLOWED.contains(&r.as_str()) || r.starts_with("apps/desktop/src/") {
            continue;
        }
        if has_ident(
            &code(&std::fs::read_to_string(&f).unwrap()),
            "from_card_click",
        ) {
            bad.push(r);
        }
    }
    assert!(bad.is_empty(), "builds a PersonClick: {bad:?}");
    for (name, text) in conductor_files() {
        let c = code(&text);
        if name == "click.rs" {
            assert!(c.contains("pub fn from_card_click"), "the one constructor");
        } else {
            assert!(!c.contains("PersonClick(("), "{name} builds a PersonClick");
        }
    }
}

/// The constructor is behind features that only `apps/desktop` (`card-click`) and this crate's own dev-dependency
/// (`click-test`) enable, in any Cargo.toml of the workspace.
#[test]
fn only_the_desktop_app_enables_the_click_feature() {
    let mut tomls = vec![];
    for top in ["crates", "apps", "tests", "tools"] {
        walk(&root().join(top), "toml", &mut tomls);
    }
    let mut bad = vec![];
    for f in tomls {
        let r = rel(&f);
        if f.file_name().is_some_and(|n| n == "Cargo.toml")
            && !["apps/desktop/Cargo.toml", "crates/synthesis-app/Cargo.toml"].contains(&r.as_str())
        {
            let t = std::fs::read_to_string(&f).unwrap();
            if t.contains("card-click") || t.contains("click-test") {
                bad.push(r);
            }
        }
    }
    assert!(bad.is_empty(), "click features named in: {bad:?}");
    let app = std::fs::read_to_string(root().join("crates/synthesis-app/Cargo.toml")).unwrap();
    // in the app crate they appear only as the definitions and the self dev-dependency (never in [dependencies])
    let deps = app
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("[dev-dependencies]")
        .next()
        .unwrap();
    assert!(
        !deps.contains("click"),
        "a production dependency enables a click feature"
    );
    let desk = std::fs::read_to_string(root().join("apps/desktop/Cargo.toml")).unwrap();
    assert!(desk.contains("card-click"));
}

/// Everything that can change something outside the conductor's own memory is reached from `exec.rs` alone,
/// and `exec.rs` runs a proposal only after a `PersonClick`.
#[test]
fn no_conductor_tool_can_write_outside_its_allowed_set() {
    let forbidden = [
        "Effects",
        "PersonApproved",
        "std::fs::write",
        "fs::write",
        "File::create",
        "OpenOptions",
        "create_dir",
        "remove_file",
        "remove_dir",
        "rename(",
        "Command::new",
        "std::process",
        "rusqlite",
        "Store::",
        "Writer",
        "outcome_client",
        "outcome_cmd",
        "TcpStream",
        "UnixStream",
        "reqwest",
        "set_var",
        "synthesis_session",
        "synthesis_orchestrator",
        "synthesis_sandbox",
    ];
    for (name, text) in conductor_files() {
        if name == "mod.rs" {
            assert!(!code(&text).contains("fn "), "mod.rs only re-exports");
        }
        if name == "exec.rs" || name == "mod.rs" {
            continue;
        }
        let c = code(&text);
        for f in forbidden {
            assert!(!c.contains(f), "{name} uses `{f}`");
        }
    }
    // the executor is the one file that runs an effect, and every public way in takes the click
    let exec = code(
        &conductor_files()
            .into_iter()
            .find(|(n, _)| n == "exec.rs")
            .unwrap()
            .1,
    );
    for sig in exec.split("pub async fn ").skip(1) {
        let head = sig.split(") ->").next().unwrap();
        assert!(
            head.contains("PersonClick"),
            "a public way to run a proposal without a click: {head}"
        );
    }
    assert_eq!(exec.matches("fx.").count(), 6, "one call per effect");
    // the host's own mutating methods are crate-private: only the executor reaches them
    let host = code(
        &conductor_files()
            .into_iter()
            .find(|(n, _)| n == "host.rs")
            .unwrap()
            .1,
    );
    for f in ["fn claim", "fn failed"] {
        assert!(host.contains(&format!("pub(super) {f}")), "{f}");
    }
}
