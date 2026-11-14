use std::path::Path;

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
    ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "credentials"]
        .iter()
        .any(|p| n.starts_with(p))
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
            "kubeconfig",
        ]
        .contains(&n)
        || n.ends_with(".tfvars")
        || n.starts_with("secrets.")
        || {
            // `.kube/config` compared lowercase like the rest (case-insensitive volumes)
            let mut c = path
                .components()
                .rev()
                .map(|c| c.as_os_str().to_ascii_lowercase());
            c.next().is_some_and(|f| f == "config") && c.next().is_some_and(|d| d == ".kube")
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
