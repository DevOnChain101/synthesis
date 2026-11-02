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

/// Exact secret values (and their form-encoded spelling) registered by `mask_secret`.
static MASKED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// From now on `redact_text` (and so every snapshot, console, network and tab text) replaces
/// this value. Process-wide: a value typed with `browser_fill_secret` must not come back from
/// any page capture.
pub fn mask_secret(value: &str) {
    if value.is_empty() {
        return;
    }
    let mut m = MASKED.lock().unwrap_or_else(|e| e.into_inner());
    let enc: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    for v in [value.to_owned(), enc] {
        if !m.contains(&v) {
            m.push(v);
        }
    }
}

pub fn redact_text(s: &str) -> String {
    let mut out = s.to_owned();
    for v in MASKED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        out = out.replace(v.as_str(), "[REDACTED:secret]");
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
    fn masked_secret_values_disappear_everywhere() {
        mask_secret("Zq9 pass&word!");
        let t = redact_text("value=Zq9 pass&word! enc=Zq9+pass%26word%21");
        assert!(!t.contains("Zq9") && !t.contains("pass"), "{t}");
        let u = redact_url("https://h/p?note=Zq9+pass%26word%21");
        assert!(!u.contains("Zq9"), "{u}");
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
