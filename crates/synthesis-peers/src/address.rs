//! Peer address grammar and session resolution (messaging.md section 1). An address resolves to
//! exactly one session or fails with the candidate list; there is no recency or best-match rule.

use crate::{Config, Entry};

/// `[ws:<ref>/][sess:<ref>/][agent:]<instance>` or `[ws:<ref>/]sess:<ref>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub ws: Option<String>,
    pub sess: Option<String>,
    /// Instance of the agent in that session (`main` when omitted).
    pub instance: String,
}

/// `None` when the text has no `ws:`/`sess:` component: such an address is in-session only and
/// never escapes to peers (section 1, rule 1).
pub fn parse(to: &str) -> Option<Address> {
    let to = to.trim();
    if !(to.starts_with("ws:") || to.starts_with("sess:")) {
        return None;
    }
    let (mut ws, mut sess, mut inst) = (None, None, None);
    for part in to.split('/') {
        let ok = |v: &str| !v.is_empty() && v.chars().all(|c| !c.is_whitespace() && c != '/');
        match part.split_once(':') {
            Some(("ws", v)) if ok(v) && ws.is_none() && sess.is_none() && inst.is_none() => {
                ws = Some(v.to_string());
            }
            Some(("sess", v)) if ok(v) && sess.is_none() && inst.is_none() => {
                sess = Some(v.to_string());
            }
            Some(("agent", v)) if ok(v) && inst.is_none() => inst = Some(v.to_string()),
            // a bare instance name, only as the last component
            None if ok(part) && inst.is_none() => inst = Some(part.to_string()),
            _ => return None,
        }
    }
    Some(Address {
        ws,
        sess,
        instance: inst.unwrap_or_else(|| "main".into()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    /// `not_found` | `ambiguous`
    pub kind: &'static str,
    /// `canonical (display)` of up to 10 candidates.
    pub candidates: Vec<String>,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: candidates: {}",
            self.kind,
            self.candidates.join("; ")
        )
    }
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `name` matches exactly, or `token` is a hex prefix (at least `min`) of `id`.
fn hit(token: &str, name: &str, id: &str, min: usize) -> bool {
    token == name || (token.len() >= min && is_hex(token) && id.starts_with(&token.to_lowercase()))
}

/// Sessions `a` names (ws/sess components) among the permitted `entries`, before the
/// exactly-one rule; never `self_session`.
pub fn candidates<'a>(
    entries: &'a [Entry],
    config: &Config,
    self_session: &str,
    a: &Address,
) -> Vec<&'a Entry> {
    let ok = |e: &&Entry| e.session_id != self_session && config.permits(&e.addr("main", ""));
    entries
        .iter()
        .filter(ok)
        .filter(|e| {
            a.ws.as_deref()
                .is_none_or(|w| hit(w, &e.workspace_label, &e.workspace_key, 6))
                && a.sess.as_deref().is_none_or(|s| {
                    hit(s, e.session_name.as_deref().unwrap_or(""), &e.session_id, 8)
                })
        })
        .collect()
}

/// The one live session `a` names among `entries` that `config` permits, never `self_session`.
pub fn resolve<'a>(
    entries: &'a [Entry],
    config: &Config,
    self_session: &str,
    a: &Address,
) -> Result<&'a Entry, ResolveError> {
    let all: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.session_id != self_session && config.permits(&e.addr("main", "")))
        .collect();
    let hits = candidates(entries, config, self_session, a);
    let list = |v: &[&Entry]| {
        v.iter()
            .take(10)
            .map(|e| {
                let ad = e.addr(&a.instance, "");
                format!("{} ({})", ad.canonical(), ad.display())
            })
            .collect()
    };
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(ResolveError {
            kind: "not_found",
            candidates: list(&all),
        }),
        many => Err(ResolveError {
            kind: "ambiguous",
            candidates: list(many),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(sid: &str, name: Option<&str>, label: &str, key: &str) -> Entry {
        Entry {
            v: 1,
            session_id: sid.into(),
            session_name: name.map(Into::into),
            workspace_key: key.into(),
            workspace_label: label.into(),
            workspace_root: format!("/w/{label}"),
            cwd: String::new(),
            pid: 1,
            pid_start: None,
            repo_key: String::new(),
            about: None,
            endpoint: String::new(),
            started_at: 0,
        }
    }

    fn all_ws() -> Config {
        Config::from_settings(
            serde_json::json!({"enabled": true, "allow": ["ws:api", "ws:web", "ws:abcdef"]})
                .as_object()
                .unwrap(),
            "/x",
        )
    }

    #[test]
    fn grammar() {
        assert_eq!(parse("developer"), None);
        assert_eq!(parse("agent:developer"), None);
        let a = parse("ws:api/sess:backend/agent:dev").unwrap();
        assert_eq!(
            (a.ws.as_deref(), a.sess.as_deref(), a.instance.as_str()),
            (Some("api"), Some("backend"), "dev")
        );
        assert_eq!(parse("sess:3c9e01aa").unwrap().instance, "main");
        assert_eq!(parse("ws:api/developer").unwrap().instance, "developer");
        assert_eq!(parse("sess:a/ws:b"), None);
        assert_eq!(parse("ws:"), None);
    }

    #[test]
    fn resolves_one_or_lists_candidates() {
        let es = vec![
            e("aaaaaaaa-1", Some("backend"), "api", "111111111111aaaa"),
            e("bbbbbbbb-2", Some("backend"), "web", "222222222222bbbb"),
            e("cccccccc-3", None, "api", "333333333333cccc"),
            e("dddddddd-4", Some("x"), "secret", "444444444444dddd"),
        ];
        let c = all_ws();
        let r = |s| resolve(&es, &c, "self", &parse(s).unwrap());
        assert_eq!(r("ws:web/sess:backend").unwrap().session_id, "bbbbbbbb-2");
        assert_eq!(r("sess:backend").unwrap_err().kind, "ambiguous");
        let amb = r("ws:api/agent:developer").unwrap_err();
        assert_eq!(amb.kind, "ambiguous");
        assert_eq!(amb.candidates.len(), 2);
        assert!(amb.candidates[0].starts_with("ws:111111111111/sess:aaaaaaaa-1/agent:developer ("));
        assert_eq!(r("sess:cccccccc").unwrap().session_id, "cccccccc-3");
        assert_eq!(r("sess:nobody").unwrap_err().kind, "not_found");
        // not permitted by the allow list: not even a candidate
        assert_eq!(r("ws:secret").unwrap_err().kind, "not_found");
        assert!(
            r("ws:secret")
                .unwrap_err()
                .candidates
                .iter()
                .all(|c| !c.contains("dddddddd"))
        );
        // never the caller's own session
        assert_eq!(
            resolve(&es, &c, "cccccccc-3", &parse("sess:cccccccc").unwrap())
                .unwrap_err()
                .kind,
            "not_found"
        );
    }

    #[test]
    fn two_roots_with_one_label_and_hex_collisions_are_ambiguous() {
        let es = vec![
            e("aaaaaaaa-1", None, "api", "abcdef000000000000000001"),
            e("bbbbbbbb-2", None, "api", "123456000000000000000002"),
            e(
                "cccccccc-3",
                Some("abcdef"),
                "web",
                "999999000000000000000003",
            ),
        ];
        let c = all_ws();
        let r = |s| resolve(&es, &c, "self", &parse(s).unwrap());
        assert_eq!(r("ws:api").unwrap_err().kind, "ambiguous");
        // a key prefix resolves one of them
        assert_eq!(r("ws:abcdef").unwrap().session_id, "aaaaaaaa-1");
        // a pure-hex session name that is also an id prefix elsewhere is not guessed
        let es2 = vec![
            e("abcdef12-1", None, "api", "k1"),
            e("zzzz-2", Some("abcdef12"), "api", "k2"),
        ];
        let a = parse("sess:abcdef12").unwrap();
        assert_eq!(resolve(&es2, &c, "self", &a).unwrap_err().kind, "ambiguous");
    }
}
