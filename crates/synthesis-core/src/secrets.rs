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
        || path.ends_with(".kube/config")
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

#[cfg(test)]
mod tests {
    use super::*;

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
