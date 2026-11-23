use std::path::Path;

/// Source and documentation files: a name like `secrets.rs` is code that talks about secrets.
fn is_source_name(lower_name: &str) -> bool {
    lower_name.rsplit_once('.').is_some_and(|(_, e)| {
        matches!(
            e,
            "rs" | "py"
                | "js"
                | "mjs"
                | "cjs"
                | "jsx"
                | "ts"
                | "tsx"
                | "go"
                | "java"
                | "kt"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "hpp"
                | "cs"
                | "rb"
                | "php"
                | "swift"
                | "scala"
                | "sh"
                | "md"
                | "rst"
                | "html"
                | "css"
        )
    })
}

/// Names whose content must not be copied into snapshots or prompts: dotenv files (not
/// `.example`/`.sample`/`.template`), private keys, certificates/keystores, cloud, kube,
/// terraform-vars and package-manager credential files. Name based, case-insensitive.
pub fn is_secret_path(path: &Path) -> bool {
    let Some(n) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // NTFS: `.env::$DATA` and `.env:stream` are the same file; Windows drops trailing dots/spaces.
    let n = n.to_ascii_lowercase();
    let n = n.split(':').next().unwrap_or("");
    let n = n.trim_end_matches(['.', ' ']);
    if n == ".env" || n == ".envrc" || n.starts_with(".env.") {
        return !(n.ends_with(".example") || n.ends_with(".sample") || n.ends_with(".template"));
    }
    // `secrets.rs` / `credentials.md` are the agent's own source and docs, not data
    let code = is_source_name(n);
    (["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
        .iter()
        .any(|p| n.starts_with(p))
        || (!code && n.starts_with("credentials")))
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|x| n.ends_with(x))
        || [
            ".netrc",
            "_netrc",
            ".npmrc",
            ".pgpass",
            ".git-credentials",
            ".htpasswd",
            ".dockerconfigjson",
            ".dockercfg",
            "packed-refs",
            ".vault-token",
            ".boto",
            ".s3cfg",
            ".terraformrc",
            "terraform.rc",
            ".pypirc",
            "kubeconfig",
        ]
        .contains(&n)
        || n.ends_with(".tfvars")
        || n.ends_with(".tfstate")
        || n.contains(".tfstate.")
        || n.ends_with(".kdbx")
        || n.ends_with(".kdb")
        || (!code && n.starts_with("secrets."))
        || {
            // `<dir>/<file>` pairs compared lowercase like the rest (case-insensitive volumes):
            // kube and docker client configs, `gh` hosts, and `.git/config` (remote URLs can carry a token)
            let mut c = path
                .components()
                .rev()
                .map(|c| c.as_os_str().to_ascii_lowercase());
            let (f, d) = (c.next(), c.next());
            matches!(
                (
                    f.as_deref().and_then(|s| s.to_str()),
                    d.as_deref().and_then(|s| s.to_str())
                ),
                (Some("config"), Some(".kube" | ".git"))
                    | (Some("config.json"), Some(".docker"))
                    | (Some("hosts.yml" | "oauth_token"), Some("gh"))
                    | (
                        Some(
                            "credentials.db"
                                | "access_tokens.db"
                                | "application_default_credentials.json"
                        ),
                        Some("gcloud")
                    )
                    | (Some("hub" | "config.yml"), Some("hub" | "glab-cli"))
            )
        }
}

/// A secret file by name, or anything under a credential directory (`.ssh`, `.aws`, `.gnupg`),
/// compared lowercase. For readers that walk trees (Grep) and for the responder's read ban.
pub fn is_secret_shaped(path: &Path) -> bool {
    is_secret_path(path)
        || path.components().any(|c| {
            c.as_os_str().to_str().is_some_and(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    ".ssh" | ".aws" | ".gnupg" | ".git-credentials"
                )
            })
        })
}

/// Directories whose data files are secret, but only BELOW the workspace root: a project that
/// lives under `/home/u/secrets/` or `C:\\secrets\\` is still readable. (Not `private`: macOS
/// paths start with /private.)
const SECRET_DATA_DIRS: [&str; 4] = ["secrets", ".terraform", ".kube", ".azure"];

/// [`is_secret_shaped`], plus data files below a [`SECRET_DATA_DIRS`] directory. Only the
/// components after `root` count; a path outside `root` counts whole. Component matching is
/// case-insensitive, like the names.
pub fn is_secret_shaped_in(path: &Path, root: &Path) -> bool {
    if is_secret_shaped(path) {
        return true;
    }
    let lower = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
            .collect()
    };
    let (p, r) = (lower(path), lower(root));
    let rel = if p.starts_with(&r[..]) {
        &p[r.len()..]
    } else {
        &p[..]
    };
    let Some((file, dirs)) = rel.split_last() else {
        return false;
    };
    dirs.iter().any(|d| SECRET_DATA_DIRS.contains(&d.as_str())) && !is_source_name(file)
}

/// [`is_secret_target`] relative to a workspace root (the path and its real path).
pub fn is_secret_target_in(path: &Path, root: &Path) -> bool {
    is_secret_shaped_in(path, root)
        || std::fs::canonicalize(path).is_ok_and(|r| {
            is_secret_shaped_in(
                &r,
                &std::fs::canonicalize(root).unwrap_or(root.to_path_buf()),
            )
        })
}

/// Secret-shaped by its own name or through a link (the real path). The model-facing readers
/// (Read, Grep, Glob) refuse such a file with no override.
pub fn is_secret_target(path: &Path) -> bool {
    is_secret_shaped(path) || std::fs::canonicalize(path).is_ok_and(|r| is_secret_shaped(&r))
}

/// `*`/`?` wildcard match of `name` against `pat` (shell style, no classes).
fn wild(pat: &[u8], name: &[u8]) -> bool {
    match pat.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|i| wild(rest, &name[i..])),
        Some((b'?', rest)) => !name.is_empty() && wild(rest, &name[1..]),
        Some((c, rest)) => name.first() == Some(c) && wild(rest, &name[1..]),
    }
}

/// Does the last segment of a path or glob name a secret file, or could its wildcards match one
/// (`.env*`, `*.pem`, `.e*`)? A segment of wildcards only (`*`) does not count, and like a shell a
/// leading `*`/`?` does not match a leading dot.
pub fn names_secret(segment_or_path: &str) -> bool {
    let seg = segment_or_path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("");
    if is_secret_path(Path::new(seg)) {
        return true;
    }
    if !seg.contains(['*', '?']) || seg.chars().all(|c| c == '*' || c == '?') {
        return false;
    }
    let pat = seg.to_ascii_lowercase();
    [
        ".env",
        ".env.local",
        ".envrc",
        "id_rsa",
        "id_ed25519",
        "credentials",
        "a.pem",
        "a.key",
        "a.p12",
        "a.pfx",
        "a.jks",
        "a.keystore",
        "a.tfvars",
        ".netrc",
        ".npmrc",
        ".pgpass",
        ".git-credentials",
        ".htpasswd",
    ]
    .iter()
    .any(|n| (pat.starts_with('.') || !n.starts_with('.')) && wild(pat.as_bytes(), n.as_bytes()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn state_files_credential_stores_and_dirs_are_secret_shaped() {
        for p in [
            "a/terraform.tfstate",
            "a/terraform.tfstate.backup",
            "a/prod.TFSTATE",
            "h/.dockercfg",
            "h/Vault.kdbx",
            "a/.git/packed-refs",
            "h/.vault-token",
            "h/.config/gcloud/credentials.db",
            "h/.config/glab-cli/config.yml",
            "a/.terraform/terraform.tfstate", // by its own name
        ] {
            assert!(super::is_secret_shaped(std::path::Path::new(p)), "{p}");
        }
        // the case-folding the other names get applies here too
        assert!(super::is_secret_shaped(std::path::Path::new("a/ID_RSA")));
        for p in [
            "a/private/x.txt",
            "/private/var/x",
            "a/terraform.tf",
            "a/src/state.rs",
        ] {
            assert!(!super::is_secret_shaped(std::path::Path::new(p)), "{p}");
        }
        let d = std::env::temp_dir().join(format!("sg-target-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let real = d.join("x.pem");
        std::fs::write(&real, "k").unwrap();
        assert!(super::is_secret_target(&real));
        #[cfg(unix)]
        {
            let link = d.join("innocent.txt");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(super::is_secret_target(&link), "a link to a key is a key");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn own_source_and_docs_named_like_secrets_are_readable() {
        for p in [
            "crates/x/src/secrets.rs",
            "crates/x/src/Credentials.RS",
            "docs/credentials.md",
            "a/secrets.py",
        ] {
            assert!(!super::is_secret_shaped(Path::new(p)), "{p}");
        }
        for p in [
            "a/secrets.yaml",
            "a/credentials.json",
            "a/credentials",
            "a/secrets.env",
        ] {
            assert!(super::is_secret_shaped(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn secret_dirs_count_only_below_the_workspace_root() {
        let root = Path::new("/home/u/secrets/proj");
        // the workspace's own ancestors do not make its files secret
        assert!(!super::is_secret_shaped_in(&root.join("src/main.rs"), root));
        assert!(!super::is_secret_shaped_in(&root.join("src/lib.txt"), root));
        // below the root: data files are secret, source and docs are not
        for p in [
            "secrets/db.yaml",
            ".terraform/x.json",
            ".kube/cache/x",
            ".azure/t.json",
            "tests/SECRETS/data.txt",
        ] {
            assert!(super::is_secret_shaped_in(&root.join(p), root), "{p}");
        }
        for p in ["tests/secrets/helper.rs", "tests/secrets/README.md"] {
            assert!(!super::is_secret_shaped_in(&root.join(p), root), "{p}");
        }
        // outside the root the whole path counts
        assert!(super::is_secret_shaped_in(
            Path::new("/home/u/.kube/cache/x"),
            root
        ));
        // Windows spelling: case differs between the root and the path
        let wroot = Path::new(r"C:\Users\U\Secrets\Proj");
        let f = Path::new(r"c:\users\u\secrets\proj\src\a.txt");
        if cfg!(windows) {
            assert!(!super::is_secret_shaped_in(f, wroot));
            assert!(super::is_secret_shaped_in(
                &wroot.join("SECRETS").join("k.txt"),
                wroot
            ));
        }
    }

    #[test]
    fn token_carrying_client_configs_are_secret_paths() {
        for p in [
            "a/.pypirc",
            "a/.git/config",
            "a/.GIT/Config",
            "h/.docker/config.json",
            "h/.config/gh/hosts.yml",
            "h/.kube/config",
        ] {
            assert!(super::is_secret_path(std::path::Path::new(p)), "{p}");
        }
        for p in [
            "a/config",
            "a/src/config.json",
            "a/.git/HEAD",
            "a/hosts.yml",
        ] {
            assert!(!super::is_secret_path(std::path::Path::new(p)), "{p}");
        }
    }

    use super::*;

    #[test]
    fn globs_that_could_name_a_secret() {
        for (p, want) in [
            (".env", true),
            ("a/.env*", true),
            (".e*", true),
            ("*.pem", true),
            ("**/*.key", true),
            ("id_*", true),
            ("*", false),
            ("**", false),
            ("*.rs", false),
            ("*.json", false),
            ("src/*.txt", false),
            ("main.rs", false),
            (".gitignore", false),
        ] {
            assert_eq!(names_secret(p), want, "{p}");
        }
    }

    #[test]
    fn table() {
        for (p, want) in [
            (".env", true),
            (".envrc", true),
            (".ENV:stream", true),
            (".env::$DATA", true),
            (".env.", true),
            ("Secrets.YAML", true),
            ("secrets.yml", true),
            (".env.example", false),
            (".ENV.local", true),
            ("a/b/.env.production", true),
            (".env.example", false),
            (".env.sample", false),
            (".env.template", false),
            ("environment.rs", false),
            ("id_rsa", true),
            ("id_rsa.pub", true),
            ("id_ed25519", true),
            ("credentials", true),
            ("credentials.json", true),
            ("server.PEM", true),
            ("tls.key", true),
            ("a.p12", true),
            ("a.pfx", true),
            ("a.keystore", true),
            ("a.jks", true),
            (".netrc", true),
            ("_netrc", true),
            (".npmrc", true),
            (".pgpass", true),
            (".git-credentials", true),
            ("prod.tfvars", true),
            ("secrets.yaml", true),
            (".htpasswd", true),
            (".dockerconfigjson", true),
            ("kubeconfig", true),
            ("home/.kube/config", true),
            ("home/.KUBE/Config", true),
            ("config", false),
            ("README.md", false),
            ("keys.txt", false),
            ("src/main.rs", false),
            ("", false),
            ("/", false),
        ] {
            assert_eq!(is_secret_path(Path::new(p)), want, "{p}");
        }
    }
}
