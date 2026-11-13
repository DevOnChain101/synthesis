//! Files that DEFINE what a build, test or lint command runs (scripts, recipes, plugin configs).
//! Shared by the permission engine (project code is trusted only while these are untouched by the
//! agent) and, by design, the verification gate (`synthesis-verification` plan inference), so one
//! list decides which commands run code the agent can rewrite. Names are matched lowercase on the
//! file name; `PREFIXES` match the start of the name (`vite.config.ts`).

/// Exact file names (lowercase), at any depth.
pub const FILES: &[&str] = &[
    "package.json",
    "makefile",
    "gnumakefile",
    "justfile",
    "taskfile.yml",
    "taskfile.yaml",
    "rakefile",
    "conftest.py",
    "build.rs",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "tox.ini",
    "noxfile.py",
    "pytest.ini",
    "cargo.toml",
    "go.mod",
    "rust-toolchain",
    "rust-toolchain.toml",
    "clippy.toml",
    ".clippy.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    "ruff.toml",
    ".ruff.toml",
    ".flake8",
    "mypy.ini",
    ".gitlab-ci.yml",
    "dockerfile",
    "docker-compose.yml",
    "docker-compose.yaml",
    "cmakelists.txt",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "pom.xml",
    "gemfile",
    "composer.json",
    "deno.json",
    "bunfig.toml",
    ".npmrc",
    ".yarnrc.yml",
    ".pre-commit-config.yaml",
    ".mocharc.json",
    ".mocharc.js",
    ".mocharc.yml",
    ".babelrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
];

/// File-name prefixes (lowercase): `vite.config.ts`, `tsconfig.build.json`, `lefthook.yml`.
pub const PREFIXES: &[&str] = &[
    "tsconfig",
    "jest.config",
    "jest.setup",
    "vitest.config",
    "vite.config",
    "webpack.config",
    "next.config",
    "babel.config",
    "playwright.config",
    "rollup.config",
    "ava.config",
    "karma.conf",
    "lefthook",
    "eslint.config",
    ".eslintrc",
    ".mocharc",
];

/// File-name suffixes: included makefiles.
pub const SUFFIXES: &[&str] = &[".mk"];

/// Directories whose files are definitions (config that runs code), at any depth.
pub const DIRS: &[&str] = &[".cargo", ".devcontainer", ".husky"];

/// Directories of scripts the build and test commands call. A definition for the permission
/// engine (trust cut) but NOT for the verification gate, where ordinary source can live in them.
pub const SCRIPT_DIRS: &[&str] = &["scripts", "script", "bin", "hack", "ci", "tools"];

/// Executables a check resolves through PATH (`npm run` adds `node_modules/.bin`); path prefixes.
pub const EXEC_DIRS: &[&str] = &["node_modules/.bin/", ".venv/bin/", "venv/bin/"];

/// A file that defines what the checks run (the verification gate's view; also the permission
/// engine's): names, prefixes, suffixes, config directories, CI workflows, PATH executables.
pub fn is_check_definition(path: &str) -> bool {
    let low = path.replace('\\', "/").to_lowercase();
    let mut parts: Vec<&str> = low.split('/').filter(|s| !s.is_empty()).collect();
    let Some(name) = parts.pop() else {
        return false;
    };
    FILES.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || SUFFIXES.iter().any(|s| name.ends_with(s))
        || parts.iter().any(|d| DIRS.contains(d))
        || (low.contains(".github/workflows/")
            && (name.ends_with(".yml") || name.ends_with(".yaml")))
        || EXEC_DIRS.iter().any(|d| low.starts_with(d))
}

/// `is_check_definition` plus the script directories: what cuts the project-code trust.
pub fn is_definition(path: &str) -> bool {
    if is_check_definition(path) {
        return true;
    }
    let low = path.replace('\\', "/").to_lowercase();
    let mut parts: Vec<&str> = low.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop();
    parts.iter().any(|d| SCRIPT_DIRS.contains(d))
}

#[cfg(test)]
mod tests {
    use super::is_definition;

    #[test]
    fn names_and_dirs() {
        for p in [
            "package.json",
            "a/Makefile",
            "x\\Dockerfile",
            "vite.config.ts",
            ".cargo/config.toml",
            "a/.devcontainer/devcontainer.json",
            "playwright.config.js",
            "pom.xml",
            "Gemfile",
            "scripts/ci.sh",
        ] {
            assert!(is_definition(p), "{p}");
        }
        for p in ["src/main.rs", "README.md", "a/config.toml", "package.jsonx"] {
            assert!(!is_definition(p), "{p}");
        }
    }
}
