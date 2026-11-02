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

/// Opens once (no symlink followed) and judges the open descriptor, so the file cannot be
/// swapped between the check and the read.
fn read_file(p: &std::path::Path, src: &Source) -> Result<String> {
    use std::io::Read;
    let e = |m: &str| refused(format!("secret file {src}: {m}"));
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let f = o.open(p).map_err(|_| {
        e("cannot be opened (a symlink is refused; point the source at the real file)")
    })?;
    let m = f.metadata().map_err(|_| e("cannot be read"))?;
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
    let mut s = String::new();
    f.take(MAX_SECRET_BYTES + 1)
        .read_to_string(&mut s)
        .map_err(|_| e("cannot be read as UTF-8 text"))?;
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

/// Runs INSIDE the target element's frame, in an isolated world, as one `Runtime.callFunctionOn` (`this` = the element;
/// arguments: allowed origins, field kind, value). Verification and the write happen in the same
/// JavaScript task, so the page cannot navigate or restyle between check and write. The value is
/// set through the native setter and announced with `input`/`change` events.
pub const FILL_JS: &str = r#"function(allowed, kind, value) {
  const e = this, d = e.ownerDocument, w = d.defaultView;
  const no = m => ({ok: false, error: m});
  let top;
  const frame = String(w.origin);
  try { top = String(w.top.origin); } catch (_) { return no('the field is in a frame of another origin; cross-origin frames are refused'); }
  if (frame !== top) return no('the field is in a frame of another origin (' + frame + '); cross-origin frames are refused');
  if (!allowed.includes(top)) return no('the page origin ' + top + ' is not one this secret is bound to');
  const tag = e.tagName.toLowerCase(), type = String(e.type || '').toLowerCase();
  const typeable = (tag === 'input' && ['text', 'password', 'email', 'search', 'tel', 'url', 'number', ''].includes(type)) || tag === 'textarea';
  if (!typeable) return no('the target is not a text input or textarea');
  if (e.disabled || e.readOnly) return no('the field is not enabled and editable');
  const pw = type === 'password';
  if (kind === 'password' && !pw) return no('a password secret goes only into an input of type=password');
  if ((kind === 'username' || kind === 'totp') && pw) return no('this secret is not a password; the field is type=password');
  const f = e.form;
  if (f) {
    const org = u => { try { return new URL(u, d.baseURI).origin; } catch (_) { return 'null'; } };
    const act = f.getAttribute('action');
    const targets = [act === null || act === '' ? d.URL : act];
    for (const c of f.elements) { const fa = c.getAttribute && c.getAttribute('formaction'); if (fa) targets.push(fa); }
    for (const t of targets) if (!allowed.includes(org(t))) return no('the form would submit to another origin (' + org(t) + ')');
  }
  e.scrollIntoView({block: 'center', inline: 'center'});
  const r = e.getBoundingClientRect();
  if (r.width < 8 || r.height < 8) return no('the field is too small to be a real input');
  if (r.left < 0 || r.top < 0 || r.right > w.innerWidth || r.bottom > w.innerHeight) return no('the field is not inside the viewport');
  for (let n = e; n; n = n.parentElement || (n.getRootNode && n.getRootNode().host)) {
    const cs = w.getComputedStyle(n);
    if (cs.display === 'none' || cs.visibility === 'hidden' || Number(cs.opacity) < 0.1) return no('the field or an ancestor is hidden');
    if (cs.clipPath !== 'none' || (cs.position === 'absolute' && cs.clip !== 'auto')) return no('the field or an ancestor is clipped');
    if (n !== e && (cs.overflowX !== 'visible' || cs.overflowY !== 'visible')) {
      const q = n.getBoundingClientRect();
      if (r.left < q.left - 1 || r.top < q.top - 1 || r.right > q.right + 1 || r.bottom > q.bottom + 1) return no('the field is clipped by an ancestor');
    }
  }
  const root = e.getRootNode();
  const hit = (root.elementFromPoint ? root : d).elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  if (hit !== e && !(hit && e.contains(hit))) return no('another element covers the field');
  e.focus();
  if (d.activeElement !== e && root.activeElement !== e) return no('the field could not be focused');
  const proto = tag === 'textarea' ? w.HTMLTextAreaElement.prototype : w.HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, 'value').set.call(e, value);
  try {
    e.dispatchEvent(new w.Event('input', {bubbles: true}));
    e.dispatchEvent(new w.Event('change', {bubbles: true}));
  } catch (_) { return {ok: true, maybe: true}; }
  return {ok: true};
}"#;

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
}
