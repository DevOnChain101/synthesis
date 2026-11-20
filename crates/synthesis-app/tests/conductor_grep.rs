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

/// The text outside `//` comments and `#[cfg(test)]` modules.
fn code(s: &str) -> String {
    let s = s.split("#[cfg(test)]").next().unwrap_or(s);
    s.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn no_conductor_module_constructs_the_persons_click() {
    let needle = ["PersonClick", "::from_card_click"].concat();
    let mut seen = 0;
    for (name, text) in conductor_files() {
        let c = code(&text);
        if name == "click.rs" {
            assert!(c.contains("pub fn from_card_click"), "the one constructor");
            seen += 1;
            continue;
        }
        assert!(
            !c.contains(&needle) && !c.contains("PersonClick(("),
            "{name} builds a PersonClick"
        );
    }
    assert_eq!(seen, 1);
    // nor does any other file of the crate's sources (the card handler lives in the desktop app)
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("conductor/click.rs")
            {
                assert!(
                    !code(&std::fs::read_to_string(&p).unwrap()).contains(&needle),
                    "{}",
                    p.display()
                );
            }
        }
    }
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
