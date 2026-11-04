//! Persistent login profiles (docs/BROWSER-LOGIN.md): a dedicated Chrome user-data-dir under
//! `<SYNT_HOME>/browser-profiles/<name>`. Never the person's own Chrome profile and never a
//! path that came from the model: the only input is a validated name.

use crate::discover::Os;
use crate::error::{BrowserError, Result};
use crate::policy::ip_is_private;
use std::fs::File;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use synthesis_config::Env;
use url::{Host, Url};

/// Per-profile record of the registrable domains the person logged in to (no credentials).
pub const META_FILE: &str = "synt-profile.json";
/// Created by `synt browser done`; ends a headless login wait.
pub const DONE_FILE: &str = "synt-login-done";
/// Exists only while a `browser_login` waits; `synt browser done` refuses without it.
pub const WAITING_FILE: &str = "synt-login-waiting";
const LOCK_FILE: &str = "synt-profile.lock";

fn err(m: impl Into<String>) -> BrowserError {
    BrowserError::Launch(m.into())
}

/// `[a-z0-9-]`, 1..=48 characters, no leading or trailing `-`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = (1..=48).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(err(format!(
            "invalid profile name {name:?}: use 1-48 characters from a-z, 0-9 and '-' (not at the ends)"
        )))
    }
}

/// Profile name derived from the origin host (`app.example.com` -> `app-example-com`).
pub fn default_name(origin: &Url) -> String {
    let host = origin.host_str().unwrap_or("site");
    let mut s = String::new();
    for c in host.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if c != '-' || !s.ends_with('-') {
            s.push(c);
        }
    }
    let s = s.trim_matches('-');
    let s: String = s.chars().take(48).collect();
    let s = s.trim_end_matches('-');
    if s.is_empty() {
        "site".into()
    } else {
        s.into()
    }
}

pub fn profiles_root(home: &Path) -> PathBuf {
    home.join("browser-profiles")
}

/// The directory of a validated name; nothing is created.
pub fn profile_path(home: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(profiles_root(home).join(name))
}

/// Creates `p` (0700) or checks the existing one. lstat comes BEFORE any chmod, so a symlink
/// planted at `p` is refused instead of having its target's mode changed.
fn private_dir(p: &Path) -> Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
            return Err(err(format!(
                "{} is not a plain directory (symlinks are refused)",
                p.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(p)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir_all(p)?;
        }
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // re-check after create: a race could have swapped in a symlink
        if std::fs::symlink_metadata(p)?.file_type().is_symlink() {
            return Err(err(format!("{} became a symlink", p.display())));
        }
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Creates (mode 0700) or reuses the profile directory. A symlink at either level is refused,
/// so the profile cannot be redirected outside the Synthesis home.
pub fn create(home: &Path, name: &str) -> Result<PathBuf> {
    let dir = profile_path(home, name)?;
    private_dir(&profiles_root(home))?;
    private_dir(&dir)?;
    confined(home, &dir)?;
    Ok(dir)
}

/// An existing profile directory, or an error that says to run `browser_login` first.
pub fn existing(home: &Path, name: &str) -> Result<PathBuf> {
    let dir = profile_path(home, name)?;
    if !dir.join(META_FILE).is_file() {
        return Err(err(format!(
            "browser profile {name:?} does not exist; the person must log in first with browser_login"
        )));
    }
    confined(home, &dir)?;
    Ok(dir)
}

fn confined(home: &Path, dir: &Path) -> Result<()> {
    for p in [profiles_root(home), dir.to_path_buf()] {
        let m = std::fs::symlink_metadata(&p)?;
        if m.file_type().is_symlink() || !m.is_dir() {
            return Err(err(format!(
                "{} is not a plain directory (symlinks are refused)",
                p.display()
            )));
        }
    }
    let (root, d) = (
        std::fs::canonicalize(profiles_root(home))?,
        std::fs::canonicalize(dir)?,
    );
    if d.parent() != Some(root.as_path()) {
        return Err(err("profile path escapes the profiles directory"));
    }
    Ok(())
}

fn loopback_host(h: &Host<&str>) -> bool {
    match h {
        Host::Domain(d) => *d == "localhost",
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

/// The origin a login may open: `https`, or `http` for localhost / 127.0.0.1 / `[::1]` only; no
/// userinfo, path, query or fragment; no private, link-local or metadata address except
/// loopback. DNS is checked later by the runtime's URL policy.
pub fn validate_login_origin(raw: &str) -> Result<Url> {
    let bad = |m: &str| err(format!("login origin {raw:?}: {m}"));
    let u = Url::parse(raw.trim()).map_err(|_| bad("not a URL"))?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("URLs with credentials are refused"));
    }
    let host = u.host().ok_or_else(|| bad("no host"))?;
    let loopback = loopback_host(&host);
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(bad(
                "http is allowed only for localhost, 127.0.0.1 and [::1]",
            ));
        }
        _ => return Err(bad("only https (or http on loopback) is allowed")),
    }
    if (u.path() != "/" && !u.path().is_empty()) || u.query().is_some() || u.fragment().is_some() {
        return Err(bad(
            "give an origin (scheme://host[:port]) without path, query or fragment",
        ));
    }
    let ip = match &host {
        Host::Ipv4(ip) => Some(IpAddr::V4(*ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(*ip)),
        Host::Domain(_) => None,
    };
    if !loopback && ip.is_some_and(ip_is_private) {
        return Err(bad("private and link-local addresses are refused"));
    }
    Ok(u)
}

/// True for `localhost`, `127.0.0.1` and `[::1]` origins.
pub fn is_loopback(u: &Url) -> bool {
    u.host().as_ref().is_some_and(loopback_host)
}

/// Origin string `scheme://host[:port]` of a URL (`None` for opaque origins).
pub fn origin_of(u: &Url) -> Option<String> {
    u.origin()
        .is_tuple()
        .then(|| u.origin().ascii_serialization())
}

/// Registrable domain (eTLD+1) from the public-suffix list (`psl` crate); a host without one
/// (IP literal, `localhost`, a bare suffix) is returned as is.
pub fn registrable_domain(host: &str) -> String {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.parse::<IpAddr>().is_ok() {
        return h; // psl would call 127.0.0.1 a domain "0.1"
    }
    psl::domain_str(&h).map_or(h.clone(), str::to_owned)
}

/// What the person logged in to, recorded per profile (no credentials).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub domains: Vec<String>,
    /// Ports of loopback origins (`http://localhost:3000`): the only loopback ports the profile may use.
    pub loopback_ports: Vec<u16>,
}

pub fn read_meta(dir: &Path) -> Meta {
    let v: serde_json::Value = std::fs::read(dir.join(META_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    Meta {
        domains: list("domains")
            .iter()
            .filter_map(|d| d.as_str().map(str::to_owned))
            .collect(),
        loopback_ports: list("loopback_ports")
            .iter()
            .filter_map(|d| d.as_u64().and_then(|p| u16::try_from(p).ok()))
            .collect(),
    }
}

pub fn read_domains(dir: &Path) -> Vec<String> {
    read_meta(dir).domains
}

/// Records the origin of a login that produced cookies: its registrable domain, and for a loopback
/// origin its port. Written atomically (temp file, fsync, rename).
pub fn add_domain(dir: &Path, origin: &Url) -> Result<()> {
    use std::io::Write;
    let mut m = read_meta(dir);
    let new = registrable_domain(origin.host_str().unwrap_or(""));
    if !m.domains.contains(&new) {
        m.domains.push(new);
    }
    if is_loopback(origin)
        && let Some(p) = origin.port_or_known_default()
        && !m.loopback_ports.contains(&p)
    {
        m.loopback_ports.push(p);
    }
    let body = serde_json::to_vec_pretty(&serde_json::json!({"version": 1, "domains": m.domains, "loopback_ports": m.loopback_ports}))
        .unwrap_or_default();
    let tmp = dir.join(format!("{META_FILE}.tmp"));
    let mut f = File::create(&tmp)?;
    f.write_all(&body)?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(META_FILE))?;
    Ok(())
}

/// Switches off Chrome's password manager, autofill and save prompts for a persistent profile
/// (Preferences are merged, other keys kept). Chrome would otherwise offer to store the typed
/// password in the profile, in clear reach of anyone who can read it.
pub fn harden_preferences(dir: &Path) -> Result<()> {
    let default = dir.join("Default");
    private_dir(&default)?;
    let path = default.join("Preferences");
    let mut v: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    for k in [
        "credentials_enable_service",
        "credentials_enable_autosignin",
        "profile.password_manager_enabled",
        "profile.password_manager_leak_detection",
        "autofill.profile_enabled",
        "autofill.credit_card_enabled",
        "autofill.enabled",
    ] {
        let mut cur = &mut v;
        let mut parts = k.split('.').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                cur[part] = serde_json::json!(false);
            } else {
                if !cur[part].is_object() {
                    cur[part] = serde_json::json!({});
                }
                cur = &mut cur[part];
            }
        }
    }
    let tmp = default.join("Preferences.synt-tmp");
    std::fs::write(&tmp, serde_json::to_vec(&v).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Exclusive hold on a profile; released on drop. A second browser on the same profile fails.
#[derive(Debug)]
pub struct ProfileLock(#[allow(dead_code)] File); // held only for its OS lock

pub fn lock(dir: &Path) -> Result<ProfileLock> {
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    f.try_lock().map_err(|_| {
        err(format!(
            "browser profile {} is already open in another browser; close it first",
            dir.file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned())
        ))
    })?;
    Ok(ProfileLock(f))
}

/// Can a visible browser window open? Only Linux can be known to lack one (no X11/Wayland
/// display: SSH-only shells, CI, containers); macOS and Windows are assumed to have a desktop.
pub fn display_available(env: &Env, os: Os) -> std::result::Result<(), String> {
    if os == Os::Linux
        && env.get_nonempty("DISPLAY").is_none()
        && env.get_nonempty("WAYLAND_DISPLAY").is_none()
    {
        return Err("no display (DISPLAY and WAYLAND_DISPLAY are unset: SSH, CI or a container), so a visible browser cannot open. Store the credential as a secret in your user settings (synt.browser.secrets) and use browser_fill_secret instead".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["a", "app-example-com", "x1"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "A",
            "a b",
            "../x",
            "a/b",
            "a.b",
            "a\\b",
            &"x".repeat(49),
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        let u = Url::parse("https://App.Example.com:8443").unwrap();
        assert_eq!(default_name(&u), "app-example-com");
        assert!(validate_name(&default_name(&Url::parse("http://[::1]:3000").unwrap())).is_ok());
    }

    #[test]
    fn login_origins() {
        for ok in [
            "https://app.example.com",
            "https://a.b:8443/",
            "http://localhost:3000",
            "http://127.0.0.1",
            "http://[::1]:80",
        ] {
            assert!(validate_login_origin(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.com",
            "ftp://example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "https://user:pw@example.com",
            "https://user@example.com",
            "https://10.0.0.5",
            "https://169.254.169.254",
            "https://192.168.1.1",
            "http://10.0.0.5",
            "https://example.com/path",
            "https://example.com/?q=1",
            "https://example.com/#x",
            "not a url",
            "",
        ] {
            assert!(validate_login_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn registrable() {
        assert_eq!(registrable_domain("app.example.com"), "example.com");
        assert_eq!(registrable_domain("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("alice.github.io"), "alice.github.io");
        assert_eq!(registrable_domain("a.b.example.com.au"), "example.com.au");
        assert_eq!(registrable_domain("localhost"), "localhost");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn preferences_are_hardened_merged_and_atomic() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("Default")).unwrap();
        std::fs::write(
            d.path().join("Default/Preferences"),
            r#"{"profile":{"name":"x","password_manager_enabled":true},"keep":1}"#,
        )
        .unwrap();
        harden_preferences(d.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(d.path().join("Default/Preferences")).unwrap())
                .unwrap();
        assert_eq!(v["credentials_enable_service"], false);
        assert_eq!(v["profile"]["password_manager_enabled"], false);
        assert_eq!(v["profile"]["name"], "x");
        assert_eq!(v["autofill"]["profile_enabled"], false);
        assert_eq!(v["keep"], 1);
        assert!(!d.path().join("Default/Preferences.synt-tmp").exists());
        std::fs::write(d.path().join("Default/Preferences"), "{not json").unwrap();
        harden_preferences(d.path()).unwrap(); // garbage is replaced, not trusted
    }

    #[test]
    fn meta_records_loopback_port_and_is_written_atomically() {
        let d = tempfile::tempdir().unwrap();
        add_domain(d.path(), &Url::parse("http://localhost:3000").unwrap()).unwrap();
        add_domain(d.path(), &Url::parse("https://app.example.com").unwrap()).unwrap();
        let m = read_meta(d.path());
        assert_eq!(m.domains, ["localhost", "example.com"]);
        assert_eq!(m.loopback_ports, [3000]);
        assert!(!d.path().join(format!("{META_FILE}.tmp")).exists());
    }

    #[test]
    fn profile_dir_is_confined_private_and_locked() {
        let home = tempfile::tempdir().unwrap();
        assert!(profile_path(home.path(), "../evil").is_err());
        let d = create(home.path(), "acme").unwrap();
        assert_eq!(d, profiles_root(home.path()).join("acme"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&d).unwrap().permissions().mode() & 0o777,
                0o700
            );
            // a symlinked profile directory is refused, and its target is NOT chmod-ed
            let target = tempfile::tempdir().unwrap();
            std::fs::set_permissions(target.path(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            std::os::unix::fs::symlink(target.path(), profiles_root(home.path()).join("link"))
                .unwrap();
            assert!(create(home.path(), "link").is_err());
            assert_eq!(
                std::fs::metadata(target.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
        assert!(existing(home.path(), "acme").is_err()); // no meta yet
        add_domain(&d, &Url::parse("https://app.example.com").unwrap()).unwrap();
        assert_eq!(read_domains(&d), ["example.com"]);
        assert!(existing(home.path(), "acme").is_ok());
        let l = lock(&d).unwrap();
        let second = lock(&d).unwrap_err().to_string();
        assert!(second.contains("already open"), "{second}");
        drop(l);
        // The lock is released with the file. A test thread that is forking at this very moment
        // holds a copy of the descriptor until its exec, so allow that window to close.
        let t = std::time::Instant::now();
        while lock(&d).is_err() {
            assert!(
                t.elapsed() < std::time::Duration::from_secs(5),
                "lock not released"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn no_display_on_linux_only() {
        let none = Env::from_pairs([("PATH", "/bin")]);
        let e = display_available(&none, Os::Linux).unwrap_err();
        assert!(e.contains("browser_fill_secret"));
        assert!(display_available(&none, Os::Mac).is_ok());
        assert!(display_available(&Env::from_pairs([("DISPLAY", ":0")]), Os::Linux).is_ok());
        assert!(
            display_available(
                &Env::from_pairs([("WAYLAND_DISPLAY", "wayland-0")]),
                Os::Linux
            )
            .is_ok()
        );
        assert!(display_available(&Env::from_pairs([("DISPLAY", "")]), Os::Linux).is_err());
    }
}
