//! Files that DEFINE what a build, test or lint command runs (scripts, recipes, plugin configs).
//! Shared by the permission engine (project code is trusted only while these are untouched by the
//! agent) and, by design, the verification gate (`synthesis-verification` plan inference), so one
//! list decides which commands run code the agent can rewrite. Names are matched lowercase on the
//! file name; `PREFIXES` match the start of the name (`vite.config.ts`).

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

pub const PREFIXES: &[&str] = &[
    "jest.config",
    "vitest.config",
    "vite.config",
    "webpack.config",
    "next.config",
    "babel.config",
    "playwright.config",
    "rollup.config",
    "karma.conf",
    "lefthook",
    "eslint.config",
    ".mocharc",
];

/// Directories whose files are definitions too (`.cargo/config.toml`, `.devcontainer/`).
pub const DIRS: &[&str] = &[
    ".cargo",
    ".devcontainer",
    ".husky",
    "scripts",
    "script",
    "bin",
    "hack",
    "ci",
    "tools",
];

/// `true` when `path` (any spelling) names a definition file or sits in a definition directory.
pub fn is_definition(path: &str) -> bool {
    let low = path.replace('\\', "/").to_lowercase();
    let mut parts: Vec<&str> = low.split('/').filter(|s| !s.is_empty()).collect();
    let Some(name) = parts.pop() else {
        return false;
    };
    FILES.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || parts.iter().any(|d| DIRS.contains(d))
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
