//! Secret redaction for captured page/network data (patterns follow docs/specs/events.md).

use regex::Regex;
use std::sync::OnceLock;
use url::Url;

fn patterns() -> &'static [(&'static str, Regex)] {
    static P: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    P.get_or_init(|| {
        [
            ("anthropic-key", r"sk-ant-[A-Za-z0-9_-]{8,}"),
            ("api-key", r"sk-[A-Za-z0-9_-]{20,}"),
            (
                "github-token",
                r"(?:ghp_|gho_|github_pat_)[A-Za-z0-9_]{10,}",
            ),
            ("aws-key", r"AKIA[0-9A-Z]{16}"),
            (
                "private-key",
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
            ),
            ("bearer", r"(?i)Bearer\s+[A-Za-z0-9._~+/=-]{8,}"),
        ]
        .into_iter()
        .map(|(k, p)| (k, Regex::new(p).expect("static regex")))
        .collect()
    })
}

/// Values shorter than this are not masked by content (a 3-letter "secret" would garble every
/// page it appears in, and the masking itself would tell a page whether a guess matched). The
/// capture guard in `BrowserRuntime` covers them instead.
pub const MASK_MIN: usize = 6;
/// Most secrets masked at once; the least recently typed one is dropped first.
const MASK_CAP: usize = 64;

/// One masked secret with every spelling a page could echo it in.
struct Masked {
    value: String,
    spellings: Vec<String>,
}

static MASKED: std::sync::Mutex<Vec<Masked>> = std::sync::Mutex::new(Vec::new());

fn percent(value: &str, keep: &str, upper: bool) -> String {
    let mut o = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || keep.as_bytes().contains(&b) {
            o.push(b as char);
        } else if upper {
            o.push_str(&format!("%{b:02X}"));
        } else {
            o.push_str(&format!("%{b:02x}"));
        }
    }
    o
}

/// JSON `\uXXXX` escapes of every non-ASCII character (surrogate pairs above the BMP).
fn json_unicode(value: &str, upper: bool) -> String {
    let mut o = String::new();
    for c in value.chars() {
        if c.is_ascii() {
            o.push(c);
        } else {
            let mut b = [0u16; 2];
            for u in c.encode_utf16(&mut b) {
                o.push_str(&if upper {
                    format!("\\u{u:04X}")
                } else {
                    format!("\\u{u:04x}")
                });
            }
        }
    }
    o
}

/// The raw value plus: form-urlencoded, `encodeURIComponent`, strict percent-encoding (upper and
/// lower hex), JSON string escape, HTML entities and base64 (standard/url-safe, padded or not).
fn spellings(value: &str) -> Vec<String> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let json = serde_json::to_string(value).unwrap_or_default();
    let mut v = vec![
        value.to_owned(),
        url::form_urlencoded::byte_serialize(value.as_bytes()).collect(),
        percent(value, "-_.!~*'()", true),
        percent(value, "-_.!~*'()", false),
        percent(value, "", true),
        percent(value, "", false),
        json[1..json.len().saturating_sub(1)].to_owned(),
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;"),
        value.replace('\'', "&#x27;"),
        percent(value, "-._~!$&'()*+,;=:@", true), // a URL path segment
        percent(value, "-._~!$&'()*+,;=:@", false),
        json_unicode(value, true),
        json_unicode(value, false),
        STANDARD.encode(value),
        STANDARD_NO_PAD.encode(value),
        URL_SAFE.encode(value),
        URL_SAFE_NO_PAD.encode(value),
    ];
    v.retain(|x| x.len() >= MASK_MIN);
    v.sort_by_key(|x| std::cmp::Reverse(x.len())); // longest first: no spelling hides inside another
    v.dedup();
    v
}

/// From now on `redact_text` (and so every snapshot, console, network and tab text) replaces
/// this value in all its spellings. Process-wide: a value typed with `browser_fill_secret` must
/// not come back from any page capture. Returns `false` (nothing masked) for a value shorter
/// than [`MASK_MIN`].
pub fn mask_secret(value: &str) -> bool {
    if value.len() < MASK_MIN {
        return false;
    }
    remember(&mut MASKED.lock().unwrap_or_else(|e| e.into_inner()), value);
    true
}

fn remember(m: &mut Vec<Masked>, value: &str) {
    m.retain(|x| x.value != value); // re-typing moves it to the most-recent end
    m.push(Masked {
        value: value.to_owned(),
        spellings: spellings(value),
    });
    if m.len() > MASK_CAP {
        m.remove(0);
    }
}

pub fn redact_text(s: &str) -> String {
    let mut out = s.to_owned();
    for m in MASKED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        for v in &m.spellings {
            out = out.replace(v.as_str(), "[REDACTED:secret]");
        }
    }
    for (kind, re) in patterns() {
        out = re
            .replace_all(&out, format!("[REDACTED:{kind}]").as_str())
            .into_owned();
    }
    out
}

fn is_secret_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(n.as_str(), "key" | "code" | "sid" | "sig")
        || [
            "token",
            "secret",
            "password",
            "passwd",
            "auth",
            "session",
            "credential",
            "apikey",
            "api_key",
            "api-key",
            "signature",
        ]
        .iter()
        .any(|w| n.contains(w))
}

/// Drops userinfo, masks secret-looking query values and fragments, then applies text patterns.
pub fn redact_url(raw: &str) -> String {
    let Ok(mut u) = Url::parse(raw) else {
        return redact_text(raw);
    };
    if !u.username().is_empty() || u.password().is_some() {
        let _ = u.set_username("");
        let _ = u.set_password(None);
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| {
                let v = if is_secret_name(&k) {
                    "REDACTED".into()
                } else {
                    v.into_owned()
                };
                (k.into_owned(), v)
            })
            .collect();
        u.query_pairs_mut().clear().extend_pairs(pairs);
    }
    if u.fragment().is_some_and(|f| {
        f.contains('=')
            && f.split('&')
                .any(|p| is_secret_name(p.split('=').next().unwrap_or("")))
    }) {
        u.set_fragment(Some("REDACTED"));
    }
    redact_text(u.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_patterns() {
        let s =
            redact_text("k=sk-ant-abcdefgh12345 and Bearer abcdefghijkl and AKIAABCDEFGHIJKLMNOP");
        assert!(!s.contains("abcdefgh12345"));
        assert!(s.contains("[REDACTED:anthropic-key]"));
        assert!(s.contains("[REDACTED:bearer]"));
        assert!(s.contains("[REDACTED:aws-key]"));
        assert_eq!(redact_text("plain text"), "plain text");
    }

    #[test]
    fn masked_secret_values_disappear_in_every_spelling() {
        use base64::Engine;
        let v = "Zq9 pass&word!/'<x>\"";
        assert!(mask_secret(v));
        let cases = [
            v.to_owned(),
            url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>(),
            percent(v, "-_.!~*'()", true),
            percent(v, "-_.!~*'()", false),
            percent(v, "", true),
            percent(v, "", false),
            v.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;"),
            base64::engine::general_purpose::STANDARD.encode(v),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v),
            percent(v, "-._~!$&'()*+,;=:@", true),
        ];
        for c in cases {
            let t = redact_text(&format!("before {c} after"));
            assert_eq!(t, "before [REDACTED:secret] after", "{c}");
        }
        let u = redact_url(&format!(
            "https://h/p?note={}",
            url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
        ));
        assert!(!u.contains("Zq9"), "{u}");
    }

    #[test]
    fn unicode_escapes_and_basic_auth_pairs_are_masked() {
        use base64::Engine;
        let v = "pässw\u{1F512}rd-ü";
        assert!(mask_secret(v));
        for c in [
            json_unicode(v, true),
            json_unicode(v, false),
            percent(v, "", true),
        ] {
            assert_eq!(redact_text(&format!("x{c}y")), "x[REDACTED:secret]y", "{c}");
        }
        assert!(json_unicode(v, false).contains("\\ud83d\\udd12"));
        // `Authorization: Basic base64(user:pass)` only when the pair is registered
        let auth =
            base64::engine::general_purpose::STANDARD.encode("alice@example.com:Zr7-pair-secret");
        assert_eq!(
            redact_text(&auth),
            auth,
            "a pair nobody declared is not masked"
        );
        assert!(mask_secret("alice@example.com:Zr7-pair-secret"));
        assert_eq!(redact_text(&auth), "[REDACTED:secret]");
    }

    #[test]
    fn short_values_are_not_masked_and_the_list_is_bounded() {
        assert!(!mask_secret("abc12"));
        assert_eq!(redact_text("abc12 abc12"), "abc12 abc12");
        let mut m = vec![];
        for i in 0..(MASK_CAP + 5) {
            remember(&mut m, &format!("lru-secret-value-{i:03}"));
        }
        remember(&mut m, "lru-secret-value-010"); // typed again: now the most recent
        assert_eq!(m.len(), MASK_CAP);
        let has = |v: &str| m.iter().any(|x| x.value == v);
        assert!(
            !has("lru-secret-value-000")
                && has("lru-secret-value-010")
                && has("lru-secret-value-068")
        );
    }

    #[test]
    fn url_userinfo_query_fragment() {
        let s = redact_url(
            "https://bob:pw@example.com/a?x=1&access_token=abc&keyword=ok#access_token=zzz",
        );
        assert!(!s.contains("bob") && !s.contains("pw@"));
        assert!(!s.contains("abc") && !s.contains("zzz"));
        assert!(s.contains("x=1") && s.contains("keyword=ok"));
        assert_eq!(redact_url("http://h/p?q=1"), "http://h/p?q=1");
    }
}
