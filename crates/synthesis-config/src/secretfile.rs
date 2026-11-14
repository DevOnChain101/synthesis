//! Which files a `{file:}` credential reference may read. The settings UI and the loader share it so a
//! reference cannot be pointed at another tool's secret store, a system file, a network path or something
//! that is not a small regular file (the loader checks again at use time, so a path that was fine when it
//! was saved and later became a link to `~/.ssh/id_rsa` is still refused).

use std::path::{Path, PathBuf};

/// Biggest credential file read.
pub const MAX_KEY_FILE: u64 = 4096;

fn norm(p: &Path) -> String {
    let n = p.to_string_lossy().replace('\\', "/").to_lowercase();
    // Windows verbatim forms (`\\?\C:\x`, `\\?\UNC\h\s`) name the same place as the plain form
    if let Some(r) = n.strip_prefix("//?/unc/") {
        return format!("//{r}");
    }
    n.strip_prefix("//?/").map(str::to_string).unwrap_or(n)
}

fn under(path: &str, dir: &str) -> bool {
    let d = dir.trim_end_matches('/');
    // an empty directory (a home of `/` or `""`) must not make every absolute path "inside" it
    !d.is_empty() && (path == d || path.strip_prefix(d).is_some_and(|r| r.starts_with('/')))
}

/// Paths under the user's home that hold other tools' credentials (matched as a folder, a file or, for
/// `.cargo/credentials`, a prefix: `credentials.toml`).
const SECRET_PATHS: [&str; 17] = [
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gcloud",
    ".config/gh",
    ".kube",
    ".docker",
    ".azure",
    ".oci",
    ".password-store",
    "library/keychains",
    ".netrc",
    ".npmrc",
    ".git-credentials",
    ".pypirc",
    ".cargo/credentials",
    ".local/share/keyrings",
];

const SYSTEM_DIRS: [&str; 15] = [
    "/etc",
    "/proc",
    "/sys",
    "/dev",
    "/boot",
    "/root",
    "/var/root",
    "/usr",
    "/bin",
    "/sbin",
    "/private/etc",
    "/system",
    "/library",
    "c:/windows",
    "c:/programdata",
];

/// Files of the synt home itself that are state, not keys.
const HOME_STATE: [&str; 8] = [
    "chain.key",
    "trust.key",
    "run",
    "synt.json",
    "settings.json",
    "auth.json",
    "state.json",
    "synt.db",
];

/// `Ok(path)`: `full` (made canonical when it exists) may be read as a credential. `strict_home`: under the
/// synt home only `keys/` is allowed (the form); otherwise only the state files are refused (the loader,
/// which keeps working setups like `~/.synt/mykey` as they were). Never touches a network path.
pub fn check_credential_file(
    full: &Path,
    home: &Path,
    user_home: &Path,
    strict_home: bool,
) -> Result<PathBuf, String> {
    let bad = |why: &str| Err(format!("file path: {why}"));
    let raw = full.to_string_lossy().replace('\\', "/");
    if raw.starts_with("//")
        || raw.starts_with("/dev/")
        || raw.starts_with("/proc/")
        || raw.starts_with("/sys/")
    {
        return bad("network and device paths are refused");
    }
    if raw.split('/').any(|c| c == "..") {
        return bad("no .. in the path");
    }
    let real = std::fs::canonicalize(full).ok();
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let homes = [norm(&canon(home)), norm(home)];
    let uhs = [norm(&canon(user_home)), norm(user_home)];
    for cand in std::iter::once(full).chain(real.as_deref()) {
        let n = norm(cand);
        if n.starts_with("//") {
            return bad("network and device paths are refused");
        }
        let own = uhs.iter().any(|u| under(&n, u));
        let secret = uhs.iter().any(|u| {
            SECRET_PATHS.iter().any(|d| {
                let dir = format!("{u}/{d}");
                under(&n, &dir) || (d.ends_with("credentials") && n.starts_with(&dir))
            })
        });
        // the secret stores are checked before the synt-home exemption: a synt home that contains them
        // (`SYNT_HOME=$HOME`) must not open them
        if !secret && let Some(h) = homes.iter().find(|h| under(&n, h)) {
            let rel = n[h.len()..].trim_start_matches('/').to_string();
            let first = rel.split('/').next().unwrap_or("");
            let keys = first == "keys";
            if (strict_home && !keys) || HOME_STATE.contains(&first) {
                return bad("under the synt home only files of the keys/ folder can hold a key");
            }
            continue;
        }
        // another account's home (a key of someone else) is never read
        let other_home = ["/home", "/users", "c:/users"]
            .iter()
            .any(|b| n.strip_prefix(&format!("{b}/")).is_some() && !own);
        if secret || other_home || SYSTEM_DIRS.iter().any(|d| under(&n, d)) {
            return bad("that location holds credentials or system files; use a file of its own");
        }
    }
    if let Some(r) = &real {
        let m = std::fs::metadata(r).map_err(|_| "file path: not readable".to_string())?;
        if !m.is_file() || m.len() > MAX_KEY_FILE {
            return bad("must be a regular file of at most 4 KB");
        }
    }
    Ok(real.unwrap_or_else(|| full.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_home_does_not_make_every_path_inside_it() {
        assert!(!under("/users/x/.ssh/id_rsa", ""));
        assert!(!under("/users/x/.ssh/id_rsa", "/"));
        assert!(under("/a/b", "/a"));
    }

    #[test]
    fn windows_verbatim_forms_match_the_plain_ones() {
        assert_eq!(norm(Path::new(r"\\?\C:\Windows\x")), "c:/windows/x");
        assert_eq!(norm(Path::new(r"\\?\UNC\Srv\Share")), "//srv/share");
    }

    #[test]
    fn a_synt_home_that_contains_the_secret_stores_does_not_open_them() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "k").unwrap();
        std::fs::write(home.join("mykey"), "k").unwrap();
        // SYNT_HOME == the user's home
        assert!(check_credential_file(&home.join(".ssh/id_rsa"), &home, &home, false).is_err());
        assert!(check_credential_file(&home.join("mykey"), &home, &home, false).is_ok());
    }
}
