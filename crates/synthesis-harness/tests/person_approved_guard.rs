//! `PersonApproved::for_text` is the person's "I approved exactly these bytes": no source file outside this crate
//! may build one except the approval-card code of P3b, which must be added to `ALLOWED` in the same change (and
//! reviewed as security-relevant).

use std::path::{Path, PathBuf};

/// `src` files (relative to the repo root) that may call the constructor. Only the conductor's executor, which runs after the person's click.
const ALLOWED: &[&str] = &["crates/synthesis-app/src/conductor/exec.rs"];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name()
                .is_some_and(|n| n == "target" || n == "node_modules")
            {
                continue;
            }
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_source_file_outside_the_harness_builds_a_person_approval() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let own = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let mut files = vec![];
    for top in ["crates", "apps"] {
        rs_files(&root.join(top), &mut files);
    }
    let needle = ["PersonApproved", "::for_text"].concat();
    let mut bad = vec![];
    for f in files {
        let c = f.canonicalize().unwrap();
        let rel = c
            .strip_prefix(root.canonicalize().unwrap())
            .unwrap()
            .to_path_buf();
        let in_src = rel.components().any(|x| x.as_os_str() == "src");
        if c.starts_with(&own) || !in_src || ALLOWED.iter().any(|a| rel == Path::new(a)) {
            continue;
        }
        if std::fs::read_to_string(&c).is_ok_and(|t| t.contains(&needle)) {
            bad.push(rel.display().to_string());
        }
    }
    assert!(
        bad.is_empty(),
        "not allowed to build a PersonApproved: {bad:?}"
    );
}
