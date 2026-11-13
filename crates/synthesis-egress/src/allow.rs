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
    // `1.2.3.4.` is an IP spelled with a dot: never a name (it would dodge the literal rules)
    if name.parse::<Ipv4Addr>().is_ok() {
        return None;
    }
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    ok.then_some(Target::Name(name))
}

/// Public suffixes whose subdomains belong to unrelated parties: a wildcard on one allows
/// anybody's site. A small built-in list of the common multi-tenant ones (ICANN second-level
/// registries and hosting platforms), not the full Public Suffix List: a new dependency and data
/// that goes stale for a check that only guards a typo. shortcut: extend this list, or take the
/// `psl` crate (already used by synthesis-browser), when a platform is missing.
const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk",
    "org.uk",
    "ac.uk",
    "gov.uk",
    "me.uk",
    "com.au",
    "net.au",
    "org.au",
    "co.nz",
    "co.jp",
    "or.jp",
    "ne.jp",
    "com.br",
    "com.cn",
    "com.mx",
    "com.ar",
    "com.tr",
    "com.sg",
    "com.hk",
    "co.in",
    "co.za",
    "co.kr",
    "co.il",
    "github.io",
    "githubusercontent.com",
    "gitlab.io",
    "herokuapp.com",
    "vercel.app",
    "netlify.app",
    "pages.dev",
    "workers.dev",
    "web.app",
    "firebaseapp.com",
    "appspot.com",
    "azurewebsites.net",
    "cloudfront.net",
    "amazonaws.com",
    "s3.amazonaws.com",
    "elasticbeanstalk.com",
    "blogspot.com",
    "wordpress.com",
    "fly.dev",
    "onrender.com",
    "railway.app",
    "repl.co",
    "glitch.me",
    "surge.sh",
    "ngrok.io",
    "ngrok-free.app",
    "trycloudflare.com",
    "myshopify.com",
    "cloudapp.azure.com",
    "azureedge.net",
    "r2.dev",
    "vercel.dev",
    "now.sh",
    "deno.dev",
    "netlify.com",
    "pythonanywhere.com",
    "weebly.com",
    "wixsite.com",
    "s3.us-east-1.amazonaws.com",
];

fn is_public_suffix(n: &str) -> bool {
    !n.contains('.') || PUBLIC_SUFFIXES.contains(&n)
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

    /// IP-literal entries that are private, loopback, link-local or metadata addresses: the
    /// only way a command can reach one. Shown to the person at start.
    pub fn private_ips(&self) -> Vec<(IpAddr, u16)> {
        self.0
            .iter()
            .filter_map(|e| match e.pattern {
                Pattern::Ip(ip) if synthesis_platform::net::ip_blocked(ip) => Some((ip, e.port)),
                _ => None,
            })
            .collect()
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
        // all digits: `+443` parses as a u16 otherwise
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<u16>().ok())
            .flatten()
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
            Some(Target::Name(n)) if is_public_suffix(&n) => Err(format!(
                "*.{n} spans unrelated tenants (a public suffix); name the host, or a domain you control"
            )),
            Some(Target::Name(n)) if n.contains('.') => Ok(Entry {
                pattern: Pattern::Suffix(n),
                port,
            }),
            _ => Err("a wildcard needs a domain of at least two labels (*.example.com)".into()),
        };
    }
    let bare_v6 = host.contains(':') && !host.starts_with('[');
    let parsed = if bare_v6 {
        normalise_host(&format!("[{host}]"))
    } else {
        normalise_host(host)
    };
    match parsed.ok_or("not a hostname or IP literal (use punycode)")? {
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
        for e in [
            "",
            "a:b",
            "a:0",
            "a:99999",
            "a:65536",
            "a:+443",
            "a:-1",
            "a:",
            "bücher.de",
            "[::1",
            "a b",
            "user@h:443",
            "h:443:80",
            "1.2.3.4.",
        ] {
            assert!(Allowlist::parse(&[e]).is_err(), "{e:?}");
        }
        // a bare IPv6 literal without a port means 443; with brackets too
        assert!(Allowlist::parse(&["[::1]", "::1"]).is_ok());
    }

    #[test]
    fn connect_targets_need_a_clean_port_and_host() {
        for t in [
            "h",
            "[::1]",
            "h:65536",
            "h:+443",
            "h:443:80",
            "h:",
            ":443",
            "1.2.3.4.:443",
        ] {
            let ok = split_host_port(t, None)
                .ok()
                .and_then(|(h, _)| normalise_host(h));
            assert_eq!(ok, None, "{t:?}");
        }
        assert_eq!(normalise_host("user@h"), None);
        assert_eq!(normalise_host("1.2.3.4."), None);
        assert_eq!(split_host_port("h:443", None), Ok(("h", 443)));
    }

    #[test]
    fn public_suffix_wildcards_are_refused() {
        for e in [
            "*.github.io",
            "*.co.uk",
            "*.com.au",
            "*.herokuapp.com",
            "*.pages.dev",
            "*.s3.amazonaws.com",
        ] {
            let err = Allowlist::parse(&[e]).unwrap_err();
            assert!(
                err.contains("public suffix") || err.contains("two labels"),
                "{e}: {err}"
            );
        }
        assert!(Allowlist::parse(&["*.example.com", "me.github.io"]).is_ok());
    }

    #[test]
    fn private_literals_are_listed_for_the_start_notice() {
        let l = list(&[
            "127.0.0.1:8080",
            "10.0.0.5:22",
            "1.1.1.1:443",
            "example.com",
        ]);
        assert_eq!(l.private_ips().len(), 2);
    }
}
