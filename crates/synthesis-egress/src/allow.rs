//! Allowlist entries and the host normaliser. Pure: no DNS, no I/O.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A connection target after normalisation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Lowercase ASCII (punycode for IDNs), no trailing dot.
    Name(String),
    Ip(IpAddr),
}

/// Lowercase, strip one trailing dot, reject non-ASCII (callers send punycode) and anything that
/// is not a hostname or an IP literal. `[v6]` brackets are accepted.
pub fn normalise_host(raw: &str) -> Option<Target> {
    let h = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']'));
    if let Some(v6) = h {
        return v6.parse::<Ipv6Addr>().ok().map(|a| Target::Ip(a.into()));
    }
    if !raw.is_ascii() {
        return None;
    }
    if let Ok(v4) = raw.parse::<Ipv4Addr>() {
        return Some(Target::Ip(v4.into()));
    }
    let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    ok.then_some(Target::Name(name))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Pattern {
    Name(String),
    /// `*.suffix`: any name ending in `.suffix` (a label boundary), not the suffix itself.
    Suffix(String),
    Ip(IpAddr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    pattern: Pattern,
    port: u16,
}

/// What the person allowed. Entries: `host` (port 443), `host:port`, `*.suffix[:port]`,
/// `ip:port` / `[v6]:port` (the only way a private or loopback address can be reached).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist(Vec<Entry>);

impl Allowlist {
    pub fn parse<S: AsRef<str>>(entries: &[S]) -> Result<Self, String> {
        entries
            .iter()
            .map(|e| parse_entry(e.as_ref().trim()).map_err(|m| format!("{:?}: {m}", e.as_ref())))
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Does an entry cover this target and port? Names only match name patterns, IP literals
    /// only match IP entries.
    pub fn allows(&self, t: &Target, port: u16) -> bool {
        self.0.iter().any(|e| {
            e.port == port
                && match (&e.pattern, t) {
                    (Pattern::Name(n), Target::Name(h)) => n == h,
                    (Pattern::Suffix(s), Target::Name(h)) => {
                        h.len() > s.len() + 1 && h.ends_with(s.as_str()) && {
                            h.as_bytes()[h.len() - s.len() - 1] == b'.'
                        }
                    }
                    (Pattern::Ip(a), Target::Ip(b)) => a == b,
                    _ => false,
                }
        })
    }
}

/// Split `host[:port]` / `[v6][:port]` / bare v6; the port defaults to `default`.
pub(crate) fn split_host_port(s: &str, default: Option<u16>) -> Result<(&str, u16), String> {
    let port = |p: &str| {
        p.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("bad port {p:?}"))
    };
    if let Some(rest) = s.strip_prefix('[') {
        let (h, tail) = rest.split_once(']').ok_or("unclosed [")?;
        let host = &s[..h.len() + 2];
        return match tail {
            "" => default
                .map(|d| (host, d))
                .ok_or_else(|| "missing port".into()),
            t => Ok((host, port(t.strip_prefix(':').ok_or("junk after ]")?)?)),
        };
    }
    if s.matches(':').count() > 1 {
        // bare IPv6 literal without a port
        return default.map(|d| (s, d)).ok_or_else(|| "missing port".into());
    }
    match s.split_once(':') {
        Some((h, p)) => Ok((h, port(p)?)),
        None => default.map(|d| (s, d)).ok_or_else(|| "missing port".into()),
    }
}

fn parse_entry(s: &str) -> Result<Entry, String> {
    let (host, port) = split_host_port(s, Some(443))?;
    if let Some(suffix) = host.strip_prefix("*.") {
        return match normalise_host(suffix) {
            Some(Target::Name(n)) if n.contains('.') => Ok(Entry {
                pattern: Pattern::Suffix(n),
                port,
            }),
            _ => Err("a wildcard needs a domain of at least two labels (*.example.com)".into()),
        };
    }
    match normalise_host(host).ok_or("not a hostname or IP literal (use punycode)")? {
        Target::Name(n) => Ok(Entry {
            pattern: Pattern::Name(n),
            port,
        }),
        Target::Ip(ip) => Ok(Entry {
            pattern: Pattern::Ip(ip),
            port,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Target {
        normalise_host(s).unwrap()
    }
    fn list(e: &[&str]) -> Allowlist {
        Allowlist::parse(e).unwrap()
    }

    #[test]
    fn normalises_case_trailing_dot_and_rejects_non_ascii() {
        assert_eq!(name("Example.COM."), Target::Name("example.com".into()));
        assert_eq!(name("1.2.3.4"), Target::Ip("1.2.3.4".parse().unwrap()));
        assert_eq!(name("[::1]"), Target::Ip("::1".parse().unwrap()));
        assert_eq!(normalise_host("bücher.example"), None);
        assert_eq!(normalise_host("a..b"), None);
        assert_eq!(normalise_host("a b"), None);
        assert_eq!(normalise_host(""), None);
        assert_eq!(
            name("xn--bcher-kva.example"),
            Target::Name("xn--bcher-kva.example".into())
        );
    }

    #[test]
    fn host_port_and_defaults() {
        let l = list(&["Example.com", "api.test:8443"]);
        assert!(l.allows(&name("example.com"), 443));
        assert!(l.allows(&name("EXAMPLE.com."), 443));
        assert!(!l.allows(&name("example.com"), 80));
        assert!(l.allows(&name("api.test"), 8443));
        assert!(!l.allows(&name("api.test"), 443));
        assert!(!l.allows(&name("sub.example.com"), 443));
    }

    #[test]
    fn wildcard_needs_a_label_boundary() {
        let l = list(&["*.example.com"]);
        assert!(l.allows(&name("a.example.com"), 443));
        assert!(l.allows(&name("a.b.example.com"), 443));
        assert!(!l.allows(&name("example.com"), 443));
        assert!(!l.allows(&name("evilexample.com"), 443));
        assert!(!l.allows(&name("example.com.evil.test"), 443));
        assert!(Allowlist::parse(&["*.com"]).is_err());
        assert!(Allowlist::parse(&["*"]).is_err());
    }

    #[test]
    fn ip_literals_match_only_exact_entries() {
        let l = list(&["127.0.0.1:8080", "[::1]:9", "example.com"]);
        assert!(l.allows(&name("127.0.0.1"), 8080));
        assert!(!l.allows(&name("127.0.0.1"), 8081));
        assert!(!l.allows(&name("127.0.0.2"), 8080));
        assert!(l.allows(&name("[::1]"), 9));
        // a name never matches an IP entry and vice versa
        assert!(!l.allows(&name("localhost"), 8080));
        assert!(!list(&["1.2.3.4"]).allows(&name("example.com"), 443));
    }

    #[test]
    fn bad_entries_are_rejected() {
        for e in ["", "a:b", "a:0", "a:99999", "bücher.de", "[::1", "a b"] {
            assert!(Allowlist::parse(&[e]).is_err(), "{e:?}");
        }
    }
}
