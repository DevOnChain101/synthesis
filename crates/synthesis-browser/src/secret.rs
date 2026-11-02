//! Secrets for `browser_fill_secret` (docs/BROWSER-LOGIN.md). The model names a secret; the value
//! is read here, typed into the page and never returned. Declarations come from USER settings
//! only (`synthesis.browser.secrets`), each bound to the exact origins it may be typed into.

use crate::error::{BrowserError, Result};
use crate::profile::{origin_of, validate_login_origin};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use synthesis_config::{Env, is_secret_shaped};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Password,
    Username,
    Totp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Env(String),
    File(PathBuf),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(n) => write!(f, "{{env:{n}}}"),
            Self::File(p) => write!(f, "{{file:{}}}", p.display()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretSpec {
    pub source: Source,
    /// Normalised `scheme://host[:port]` origins the value may be typed into.
    pub origins: Vec<String>,
    pub field_kind: Option<FieldKind>,
}

const MAX_SECRET_BYTES: u64 = 4096;

fn parse_source(s: &str) -> std::result::Result<Source, String> {
    if let Some(n) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        // Secret-shaped SYNTHESIS_* names are the ones `Env::child_env` never hands to commands.
        if !n.starts_with("SYNTHESIS_") || !is_secret_shaped(n) {
            return Err("an env source must be a SYNTHESIS_* variable with KEY, TOKEN, SECRET, PASSWORD or similar in its name (so it is never passed to commands the agent runs)".into());
        }
        return Ok(Source::Env(n.into()));
    }
    if let Some(p) = s.strip_prefix("{file:").and_then(|r| r.strip_suffix('}')) {
        let p = PathBuf::from(p);
        if !p.is_absolute() {
            return Err("a file source must be an absolute path".into());
        }
        return Ok(Source::File(p));
    }
    Err("source must be {env:SYNTHESIS_NAME} or {file:/absolute/path}".into())
}

/// Parses `synthesis.browser.secrets`. An invalid entry is skipped and named in the returned
/// diagnostics (never with a value).
pub fn parse_secrets(raw: &Map<String, Value>) -> (BTreeMap<String, SecretSpec>, Vec<String>) {
    let (mut out, mut diag) = (BTreeMap::new(), vec![]);
    for (name, v) in raw {
        let mut bad =
            |m: &str| diag.push(format!("synthesis.browser.secrets.{name}: {m}, skipped"));
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            bad("the name must be 1-64 characters of A-Z a-z 0-9 _ -");
            continue;
        }
        let source = match v.get("source").and_then(Value::as_str).map(parse_source) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                bad(&e);
                continue;
            }
            None => {
                bad("missing source");
                continue;
            }
        };
        let origins: Option<Vec<String>> = v
            .get("origins")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|o| {
                        let u = validate_login_origin(o.as_str()?).ok()?;
                        origin_of(&u)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(origins) = origins.filter(|o| !o.is_empty()) else {
            bad(
                "origins must be a non-empty list of https origins (http only for localhost), without path",
            );
            continue;
        };
        let field_kind = match v.get("field_kind").and_then(Value::as_str) {
            None => None,
            Some("password") => Some(FieldKind::Password),
            Some("username") => Some(FieldKind::Username),
            Some("totp") => Some(FieldKind::Totp),
            Some(_) => {
                bad("field_kind must be password, username or totp");
                continue;
            }
        };
        out.insert(
            name.clone(),
            SecretSpec {
                source,
                origins,
                field_kind,
            },
        );
    }
    (out, diag)
}

fn refused(m: String) -> BrowserError {
    BrowserError::Refused(m)
}

/// Reads the value. Errors name the source, never the value.
pub fn resolve(name: &str, spec: &SecretSpec, env: &Env) -> Result<String> {
    let v = match &spec.source {
        Source::Env(n) => env.get(n).map(str::to_owned).unwrap_or_default(),
        Source::File(p) => read_file(p, &spec.source)?,
    };
    if v.is_empty() {
        return Err(refused(format!(
            "secret {name:?}: {} is missing or empty",
            spec.source
        )));
    }
    Ok(v)
}

fn read_file(p: &std::path::Path, src: &Source) -> Result<String> {
    let e = |m: &str| refused(format!("secret file {src}: {m}"));
    let m = std::fs::symlink_metadata(p).map_err(|_| e("cannot be read"))?;
    if m.file_type().is_symlink() {
        return Err(e("is a symlink; point the source at the real file"));
    }
    if !m.is_file() {
        return Err(e("is not a regular file"));
    }
    if m.len() > MAX_SECRET_BYTES {
        return Err(e("is larger than 4096 bytes"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(e("is readable by group or others; run chmod 600 on it"));
        }
    }
    let s = std::fs::read_to_string(p).map_err(|_| e("cannot be read as UTF-8 text"))?;
    Ok(s.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(&s)
        .to_owned())
}

/// Exact scheme+host+port comparison of a page origin with the secret's origins.
pub fn origin_allowed(actual: &str, spec: &SecretSpec) -> bool {
    Url::parse(actual)
        .ok()
        .and_then(|u| origin_of(&u))
        .is_some_and(|o| spec.origins.contains(&o))
}

/// What the page says about the target element (measured inside the element's own frame).
#[derive(Debug, Default)]
pub struct Facts {
    pub tag: String,
    pub input_type: String,
    pub visible: bool,
    pub enabled: bool,
    pub readonly: bool,
    pub frame_origin: String,
    pub top_origin: String,
}

impl Facts {
    pub(crate) fn from_json(v: &Value, top_origin: &str) -> Self {
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
        Self {
            tag: s("tag"),
            input_type: s("type"),
            visible: v["visible"] == true,
            enabled: v["enabled"] == true,
            readonly: v["readonly"] == true,
            frame_origin: s("frame_origin"),
            top_origin: top_origin.to_owned(),
        }
    }
}

const TEXT_TYPES: [&str; 8] = [
    "text", "password", "email", "search", "tel", "url", "number", "",
];

/// Decides whether the value may be typed into this element. The page's top origin AND the
/// element's frame origin must both be bound to the secret.
pub fn check_target(f: &Facts, spec: &SecretSpec) -> Result<()> {
    let no = |m: String| Err(refused(m));
    if !origin_allowed(&f.top_origin, spec) {
        return no(format!(
            "the page origin {} is not one this secret is bound to",
            f.top_origin
        ));
    }
    if f.frame_origin != f.top_origin {
        return no(format!(
            "the field is in a frame of another origin ({}); cross-origin frames are refused",
            f.frame_origin
        ));
    }
    let typeable =
        (f.tag == "input" && TEXT_TYPES.contains(&f.input_type.as_str())) || f.tag == "textarea";
    if !typeable {
        return no("the target is not a text input or textarea".into());
    }
    if !f.visible || !f.enabled || f.readonly {
        return no("the field is not visible, enabled and editable".into());
    }
    let password = f.input_type == "password";
    match spec.field_kind {
        Some(FieldKind::Password) if !password => {
            no("a password secret goes only into an input of type=password".into())
        }
        Some(FieldKind::Username | FieldKind::Totp) if password => {
            no("this secret is not a password; the field is type=password".into())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(kind: Option<FieldKind>) -> SecretSpec {
        SecretSpec {
            source: Source::Env("SYNTHESIS_X_PASSWORD".into()),
            origins: vec!["https://app.example.com".into()],
            field_kind: kind,
        }
    }

    fn facts(top: &str, frame: &str, ty: &str) -> Facts {
        Facts {
            tag: "input".into(),
            input_type: ty.into(),
            visible: true,
            enabled: true,
            readonly: false,
            frame_origin: frame.into(),
            top_origin: top.into(),
        }
    }

    #[test]
    fn parses_and_rejects_declarations() {
        let raw = json!({
            "ok": {"source": "{env:SYNTHESIS_APP_PASSWORD}", "origins": ["https://app.example.com"], "field_kind": "password"},
            "file": {"source": "{file:/run/s/pw}", "origins": ["http://localhost:3000"]},
            "plain-env": {"source": "{env:HOME}", "origins": ["https://a.com"]},
            "not-secret-shaped": {"source": "{env:SYNTHESIS_APP}", "origins": ["https://a.com"]},
            "relative": {"source": "{file:pw}", "origins": ["https://a.com"]},
            "no-origins": {"source": "{env:SYNTHESIS_A_TOKEN}", "origins": []},
            "http-remote": {"source": "{env:SYNTHESIS_A_TOKEN}", "origins": ["http://a.com"]},
            "path-origin": {"source": "{env:SYNTHESIS_A_TOKEN}", "origins": ["https://a.com/login"]},
            "bad kind": {"source": "{env:SYNTHESIS_A_TOKEN}", "origins": ["https://a.com"]},
            "bad-kind": {"source": "{env:SYNTHESIS_A_TOKEN}", "origins": ["https://a.com"], "field_kind": "pin"},
        });
        let (ok, diag) = parse_secrets(raw.as_object().unwrap());
        assert_eq!(
            ok.keys().map(String::as_str).collect::<Vec<_>>(),
            ["file", "ok"]
        );
        assert_eq!(ok["ok"].field_kind, Some(FieldKind::Password));
        assert_eq!(diag.len(), 8, "{diag:?}");
        assert!(diag.iter().all(|d| !d.contains("hunter")));
    }

    #[test]
    fn env_resolution_never_names_the_value() {
        let s = spec(None);
        let e = Env::from_pairs([("SYNTHESIS_X_PASSWORD", "hunter2")]);
        assert_eq!(resolve("n", &s, &e).unwrap(), "hunter2");
        for env in [
            Env::default(),
            Env::from_pairs([("SYNTHESIS_X_PASSWORD", "")]),
        ] {
            let m = resolve("n", &s, &env).unwrap_err().to_string();
            assert!(
                m.contains("SYNTHESIS_X_PASSWORD") && m.contains("missing or empty"),
                "{m}"
            );
        }
    }

    #[test]
    fn file_sources_are_checked() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pw");
        std::fs::write(&p, "s3cret\r\n").unwrap();
        let s = SecretSpec {
            source: Source::File(p.clone()),
            ..spec(None)
        };
        let env = Env::default();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            let m = resolve("n", &s, &env).unwrap_err().to_string();
            assert!(m.contains("chmod 600") && !m.contains("s3cret"), "{m}");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            let l = d.path().join("link");
            std::os::unix::fs::symlink(&p, &l).unwrap();
            let sl = SecretSpec {
                source: Source::File(l),
                ..spec(None)
            };
            assert!(
                resolve("n", &sl, &env)
                    .unwrap_err()
                    .to_string()
                    .contains("symlink")
            );
        }
        assert_eq!(resolve("n", &s, &env).unwrap(), "s3cret");
        std::fs::write(&p, "").unwrap();
        assert!(
            resolve("n", &s, &env)
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
        let dir = SecretSpec {
            source: Source::File(d.path().to_path_buf()),
            ..spec(None)
        };
        assert!(resolve("n", &dir, &env).is_err());
    }

    #[test]
    fn origin_binding_is_exact() {
        let s = spec(None);
        assert!(origin_allowed("https://app.example.com", &s));
        assert!(origin_allowed("https://APP.example.com:443/x", &s));
        for bad in [
            "http://app.example.com",
            "https://app.example.com:8443",
            "https://evil.example.com",
            "https://app.example.com.evil.io",
            "https://example.com",
            "null",
            "",
        ] {
            assert!(!origin_allowed(bad, &s), "{bad}");
        }
    }

    #[test]
    fn target_checks() {
        let a = "https://app.example.com";
        assert!(check_target(&facts(a, a, "password"), &spec(Some(FieldKind::Password))).is_ok());
        // page moved (redirect) to another origin
        assert!(
            check_target(
                &facts("https://evil.io", "https://evil.io", "password"),
                &spec(None)
            )
            .is_err()
        );
        // cross-origin iframe inside the right page
        let m = check_target(&facts(a, "https://pay.evil.io", "password"), &spec(None))
            .unwrap_err()
            .to_string();
        assert!(m.contains("cross-origin"), "{m}");
        assert!(check_target(&facts(a, "null", "password"), &spec(None)).is_err());
        // field kind
        assert!(check_target(&facts(a, a, "text"), &spec(Some(FieldKind::Password))).is_err());
        assert!(check_target(&facts(a, a, "password"), &spec(Some(FieldKind::Username))).is_err());
        assert!(check_target(&facts(a, a, "text"), &spec(Some(FieldKind::Totp))).is_ok());
        // element state
        for tweak in [
            |f: &mut Facts| f.visible = false,
            |f: &mut Facts| f.enabled = false,
            |f: &mut Facts| f.readonly = true,
            |f: &mut Facts| f.input_type = "checkbox".into(),
            |f: &mut Facts| f.tag = "button".into(),
        ] {
            let mut f = facts(a, a, "password");
            tweak(&mut f);
            assert!(check_target(&f, &spec(None)).is_err());
        }
    }
}
