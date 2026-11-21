//! `PersonApproved::for_text` is the person's "I approved exactly these bytes". The harness feature that builds it is
//! on in every build of `synthesis-app`, so the guard is on the source: no `.rs` file anywhere (src, tests,
//! build.rs, examples, benches) may name the bare identifier `for_text` except the executor of the approval card and
//! the harness's own definition and tests. Only `synthesis-app` may enable the features in any `Cargo.toml`.

use std::path::{Path, PathBuf};

/// Files (relative to the repo root) that may name `for_text`: the executor that runs after the person's click.
const ALLOWED: &[&str] = &["crates/synthesis-app/src/conductor/exec.rs"];
/// The harness's own definition, its tests and this guard.
const HARNESS_OWN: &[&str] = &[
    "crates/synthesis-harness/src/comm.rs",
    "crates/synthesis-harness/tests/origins.rs",
    "crates/synthesis-harness/tests/person_approved_guard.rs",
];
/// `Cargo.toml` files that may name the features: the harness (definition, dev-dependency) and the product crate.
const FEATURE_TOML: &[&str] = &[
    "crates/synthesis-harness/Cargo.toml",
    "crates/synthesis-app/Cargo.toml",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name()
                .is_some_and(|n| n == "target" || n == "node_modules" || n == ".git")
            {
                continue;
            }
            files(&p, ext, out);
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.canonicalize()
        .unwrap()
        .strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// `word` as a whole identifier (not inside `ask_for_text`).
fn has_ident(text: &str, word: &str) -> bool {
    let id = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(id)
            && !text[i + word.len()..].chars().next().is_some_and(id)
    })
}

#[test]
fn no_source_file_outside_the_executor_names_for_text() {
    let root = root();
    let mut all = vec![];
    for top in ["crates", "apps", "tests", "tools"] {
        files(&root.join(top), "rs", &mut all);
    }
    let mut bad = vec![];
    for f in all {
        let r = rel(&root, &f);
        if ALLOWED.contains(&r.as_str()) || HARNESS_OWN.contains(&r.as_str()) {
            continue;
        }
        if std::fs::read_to_string(&f).is_ok_and(|t| has_ident(&t, "for_text")) {
            bad.push(r);
        }
    }
    assert!(bad.is_empty(), "not allowed to name for_text: {bad:?}");
}

#[test]
fn only_the_product_crate_enables_the_approval_card_features() {
    let root = root();
    let mut tomls = vec![root.join("Cargo.toml")];
    for top in ["crates", "apps", "tests", "tools"] {
        files(&root.join(top), "toml", &mut tomls);
    }
    let mut bad = vec![];
    for f in tomls {
        let r = rel(&root, &f);
        if f.file_name().is_some_and(|n| n == "Cargo.toml")
            && !FEATURE_TOML.contains(&r.as_str())
            && std::fs::read_to_string(&f).is_ok_and(|t| t.contains("approval-card"))
        {
            bad.push(r);
        }
    }
    assert!(bad.is_empty(), "approval-card features named in: {bad:?}");
    // the product crate turns on both (the guard above is what keeps the constructor out of other code)
    let app = std::fs::read_to_string(root.join("crates/synthesis-app/Cargo.toml")).unwrap();
    assert!(app.contains("\"approval-card-in-release\""));
}

#[test]
fn the_helper_matches_whole_identifiers_only() {
    assert!(has_ident("PersonApproved::\n  for_text(x)", "for_text"));
    assert!(has_ident("use a::for_text as f;", "for_text"));
    assert!(!has_ident("fn ask_for_text() {}", "for_text"));
}
