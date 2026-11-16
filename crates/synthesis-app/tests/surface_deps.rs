//! `deps-surfaces-do-not-import-orchestrator` (threat T-34, ADR-021): no surface depends on the
//! orchestrator crate. The apps, the CLI UI crates and the view models talk to it only through
//! `synthesis_app::outcome_client`, so they cannot reinterpret a decision, compute a verdict or
//! reach the store's write paths. Dev-dependencies (test fixtures) are not production imports and
//! are not checked.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &str = "synthesis-orchestrator";

/// The production dependency tables of a manifest that name `FORBIDDEN` (any key spelling:
/// `synthesis-orchestrator = ...`, `[dependencies.synthesis-orchestrator]`, or a renamed `package`).
fn violations(manifest: &str) -> Vec<String> {
    let mut section = String::new();
    let mut found = vec![];
    for line in manifest.lines().map(str::trim) {
        if let Some(h) = line.strip_prefix('[') {
            section = h.trim_end_matches(']').trim_matches('[').to_string();
        }
        let production = section
            .split('.')
            .any(|p| p == "dependencies" || p == "build-dependencies")
            && !section.contains("dev-dependencies");
        if !production {
            continue;
        }
        let header = line.starts_with('[');
        let names_it = (!header && line.starts_with(FORBIDDEN))
            || line.contains(&format!("package = \"{FORBIDDEN}\""))
            || (header && section.ends_with(&format!(".{FORBIDDEN}")));
        if names_it {
            found.push(format!("[{section}] {line}"));
        }
    }
    found
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn surfaces() -> Vec<PathBuf> {
    let mut out = vec![root().join("crates/synthesis-view/Cargo.toml")];
    for (dir, prefix) in [("apps", ""), ("crates", "synthesis-cli-ui")] {
        for e in fs::read_dir(root().join(dir)).unwrap().flatten() {
            let m = e.path().join("Cargo.toml");
            if m.is_file() && e.file_name().to_string_lossy().starts_with(prefix) {
                out.push(m);
            }
        }
    }
    out
}

#[test]
fn deps_surfaces_do_not_import_orchestrator() {
    let list = surfaces();
    // the scan reaches every surface it is meant to guard
    for want in ["apps/cli", "synthesis-view", "synthesis-cli-ui-render"] {
        assert!(
            list.iter().any(|p| p.to_string_lossy().contains(want)),
            "{want}: {list:?}"
        );
    }
    for m in list {
        let text = fs::read_to_string(&m).unwrap();
        assert!(
            violations(&text).is_empty(),
            "{} depends on {FORBIDDEN}: {:?}",
            m.display(),
            violations(&text)
        );
    }
}

#[test]
fn the_scan_finds_a_dependency_however_it_is_spelled() {
    for bad in [
        "[dependencies]\nsynthesis-orchestrator = { path = \"x\" }\n",
        "[dependencies.synthesis-orchestrator]\npath = \"x\"\n",
        "[target.'cfg(unix)'.dependencies]\nfoo = { package = \"synthesis-orchestrator\", path = \"x\" }\n",
        "[build-dependencies]\nsynthesis-orchestrator = \"1\"\n",
    ] {
        assert_eq!(violations(bad).len(), 1, "{bad}");
    }
    assert!(
        violations("[dev-dependencies]\nsynthesis-orchestrator = { path = \"x\" }\n").is_empty()
    );
    // the one crate that is allowed to use it does, so the scan is looking at the right thing
    let app = fs::read_to_string(root().join("crates/synthesis-app/Cargo.toml")).unwrap();
    assert_eq!(violations(&app).len(), 1);
}
