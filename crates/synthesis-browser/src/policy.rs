//! URL policy: what the browser may navigate to / request. Checked before a navigation is sent
//! and again, inside the browser, for every request (redirects, iframes, subresources).
//!
//! Hostnames are also resolved with our own resolver (`check_resolved`) and rejected when ANY
//! address is private. Best effort only: Chrome resolves again itself (own cache, possibly
//! DoH), so DNS rebinding between our lookup and Chrome's connect is not closed here; the
//! caller additionally checks the connected `remoteIPAddress` (`remote_ip_denied`), which can
//! only stop the page after the request was sent. If that event is lost (broadcast overflow)
//! the runtime tears the context down. Chrome 156's CDP has no way to pin a host to an IP
//! (checked against /json/protocol), so this residual window stays. The browser is not an SSRF
//! boundary.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::{Host, Url};

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 5 });
const CACHE_TTL: Duration = Duration::from_secs(30);
const FAILURE_TTL: Duration = Duration::from_secs(5);
const CACHE_MAX: usize = 1024;
/// Lookups running at once. A timed-out lookup keeps its blocking thread (and its permit)
/// until the OS resolver returns, so this bounds the threads a page can pin down.
const MAX_IN_FLIGHT: usize = 8;
/// Of those, kept for priority lookups (document navigations, allow-listed hosts).
const RESERVED: usize = 2;

type Lookup = Result<Vec<IpAddr>, String>;
type Cache = Mutex<HashMap<String, (Instant, Lookup)>>;
type Gates = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;
type LookupFn = dyn Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + Sync;

/// Host -> addresses. Defaults to the system resolver (`ToSocketAddrs` on a blocking thread);
/// tests inject their own. Clones share the cache.
#[derive(Clone)]
pub struct Resolver {
    lookup: Arc<LookupFn>,
    cache: Arc<Cache>,
    /// One lock per host being looked up: concurrent callers wait and then hit the cache.
    gates: Arc<Gates>,
    permits: Arc<tokio::sync::Semaphore>,
    reserved: Arc<tokio::sync::Semaphore>,
}

impl Resolver {
    pub fn new(f: impl Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + Sync + 'static) -> Self {
        Self {
            lookup: Arc::new(f),
            cache: Arc::default(),
            gates: Arc::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT - RESERVED)),
            reserved: Arc::new(tokio::sync::Semaphore::new(RESERVED)),
        }
    }

    fn cached(&self, host: &str) -> Option<Lookup> {
        let c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        c.get(host)
            .filter(|(t, r)| t.elapsed() < if r.is_ok() { CACHE_TTL } else { FAILURE_TTL })
            .map(|(_, r)| r.clone())
    }

    /// `priority`: top-level/document navigations and allow-listed hosts use a reserved pool, so
    /// subresource lookups of a hostile page cannot starve them.
    async fn resolve(&self, host: &str, priority: bool) -> Lookup {
        if let Some(r) = self.cached(host) {
            return r;
        }
        let gate = {
            let mut g = self.gates.lock().unwrap_or_else(|e| e.into_inner());
            g.entry(host.to_owned()).or_default().clone()
        };
        // Removes the gate on every exit path (return, cancel, panic).
        let release = GateRelease {
            gates: &self.gates,
            host,
            gate,
        };
        let _held = release.gate.lock().await;
        if let Some(r) = self.cached(host) {
            return r; // another caller just did the lookup
        }
        let (r, definitive) = self.lookup_once(host, priority).await;
        if definitive {
            let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if c.len() >= CACHE_MAX {
                c.clear(); // hostnames are page-controlled: bound the memory
            }
            c.insert(host.to_owned(), (Instant::now(), r.clone()));
        }
        r
    }

    /// The bool is false for timeouts and permit starvation: those fail this request closed but
    /// say nothing about the host, so they are not cached.
    async fn lookup_once(&self, host: &str, priority: bool) -> (Lookup, bool) {
        let transient = |e: String| (Err(e), false);
        let timed_out = || format!("resolving `{host}` timed out");
        let deadline = tokio::time::Instant::now() + LOOKUP_TIMEOUT;
        let pool = if priority {
            &self.reserved
        } else {
            &self.permits
        };
        let permit = match tokio::time::timeout_at(deadline, pool.clone().acquire_owned()).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return transient(e.to_string()),
            Err(_) => return transient(timed_out()),
        };
        let (f, h) = (self.lookup.clone(), host.to_owned());
        // The permit moves into the blocking task: a timeout does not cancel the thread.
        let job = tokio::task::spawn_blocking(move || {
            let r = f(&h);
            drop(permit);
            r
        });
        let ips = match tokio::time::timeout_at(deadline, job).await {
            Err(_) => return transient(timed_out()),
            Ok(Err(e)) => return transient(format!("resolver failed: {e}")),
            Ok(Ok(Err(e))) => return (Err(format!("cannot resolve `{host}`: {e}")), true),
            Ok(Ok(Ok(ips))) => ips,
        };
        if ips.is_empty() {
            return (Err(format!("`{host}` has no addresses")), true);
        }
        (Ok(ips), true)
    }
}

struct GateRelease<'a> {
    gates: &'a Gates,
    host: &'a str,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl Drop for GateRelease<'_> {
    fn drop(&mut self) {
        let mut g = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        if Arc::strong_count(&self.gate) == 2 {
            g.remove(self.host); // only the map and us hold it
        }
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new(|h| Ok((h, 0).to_socket_addrs()?.map(|a| a.ip()).collect()))
    }
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolver")
    }
}

#[derive(Clone, Debug, Default)]
pub struct UrlPolicy {
    /// When non-empty only these domains (and subdomains) are allowed.
    pub allow_domains: Vec<String>,
    pub deny_domains: Vec<String>,
    /// When non-empty, top-level navigations (not subresources) are limited to these domains and
    /// their subdomains: a login profile only browses the sites it was logged in to.
    pub document_allow_domains: Vec<String>,
    /// Loopback, RFC1918, link-local (cloud metadata 169.254.169.254) and `localhost` are
    /// blocked unless this is set. Needed to test a local dev server.
    pub allow_private_network: bool,
    /// Used by `check_resolved` to look hostnames up.
    pub resolver: Resolver,
}

fn private_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking
        || o[0] >= 240 // reserved
}

pub fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => private_v4(v),
        IpAddr::V6(v) => private_v6(v),
    }
}

fn private_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    // IPv4-mapped (::ffff:a.b.c.d), IPv4-compatible (::a.b.c.d) and NAT64 (64:ff9b::/96)
    let embedded = ip.to_ipv4_mapped().or_else(|| ip.to_ipv4()).or_else(|| {
        (seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0]).then(|| Ipv4Addr::from(ip.to_bits() as u32))
    });
    if let Some(v4) = embedded {
        return private_v4(v4);
    }
    let sixtofour = seg[0] == 0x2002
        && private_v4(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            seg[1] as u8,
            (seg[2] >> 8) as u8,
            seg[2] as u8,
        ));
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // ULA
        || (seg[0] & 0xffc0) == 0xfe80 // link-local
        || (seg[0] & 0xffc0) == 0xfec0 // site-local
        || (seg[0] == 0x2001 && seg[1] == 0) // Teredo: embeds a v4 server and an obfuscated client
        || sixtofour
}

/// Canonical form used for every comparison: lowercase, IDNA (punycode), no trailing dots,
/// IPv4 in dotted form (decimal/hex/octal accepted), IPv6 without brackets or zone id.
fn normalise(host: &str) -> String {
    let h = host.trim().trim_matches(['[', ']']);
    let h = h.split('%').next().unwrap_or(h);
    let h = h.trim_matches('.');
    let bracketed = if h.contains(':') {
        format!("[{h}]")
    } else {
        h.to_owned()
    };
    match Host::parse(&bracketed) {
        Ok(Host::Domain(d)) => d.trim_end_matches('.').to_owned(),
        Ok(Host::Ipv4(ip)) => ip.to_string(),
        Ok(Host::Ipv6(ip)) => ip.to_string(),
        Err(_) => h.to_ascii_lowercase(),
    }
}

fn matches_domain(host: &str, list: &[String]) -> bool {
    list.iter().any(|d| {
        let d = normalise(d);
        !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
    })
}

impl UrlPolicy {
    /// Top-level (and frame) navigation: only http(s) and `about:blank`.
    pub fn check_navigation(&self, raw: &str) -> Result<(), String> {
        if raw == "about:blank" {
            return Ok(());
        }
        self.check(raw, &["http", "https"])?;
        let host = Url::parse(raw)
            .ok()
            .and_then(|u| u.host().map(|h| normalise(&h.to_string())));
        match host {
            Some(h)
                if !self.document_allow_domains.is_empty()
                    && !matches_domain(&h, &self.document_allow_domains) =>
            {
                Err(format!(
                    "domain `{h}` is not one of this browser profile's domains"
                ))
            }
            _ => Ok(()),
        }
    }

    /// Subresource requests: http(s) plus data/about.
    pub fn check_request(&self, raw: &str) -> Result<(), String> {
        // ws(s), ftp and blob are never allowed: WebSocket handshakes bypass Fetch interception.
        self.check(raw, &["http", "https", "data", "about"])
    }

    /// `check_navigation` / `check_request` plus DNS: for a hostname, every address it resolves
    /// to must be public (unless `allow_private_network`). Fails closed when it cannot resolve.
    pub async fn check_resolved(&self, raw: &str, document: bool) -> Result<(), String> {
        if document {
            self.check_navigation(raw)?;
        } else {
            self.check_request(raw)?;
        }
        if self.allow_private_network {
            return Ok(());
        }
        if let Some(Host::Domain(d)) = Url::parse(raw).map_err(|e| e.to_string())?.host() {
            let name = normalise(d);
            let priority = document
                || (!self.allow_domains.is_empty() && matches_domain(&name, &self.allow_domains));
            if let Some(ip) = self
                .resolver
                .resolve(&name, priority)
                .await?
                .into_iter()
                .find(|ip| ip_is_private(*ip))
            {
                return Err(format!("`{name}` resolves to private address {ip}"));
            }
        }
        Ok(())
    }

    /// The address Chrome actually connected to (`Network.responseReceived.remoteIPAddress`,
    /// possibly bracketed) is private while the policy forbids that.
    pub fn remote_ip_denied(&self, remote: &str) -> bool {
        !self.allow_private_network
            && remote
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(ip_is_private)
    }

    fn check(&self, raw: &str, schemes: &[&str]) -> Result<(), String> {
        let u = Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
        if !schemes.contains(&u.scheme()) {
            return Err(format!("scheme `{}` is not allowed", u.scheme()));
        }
        let Some(host) = u.host() else {
            return Ok(()); // data:, blob:, about:
        };
        let name = normalise(&host.to_string());
        if matches_domain(&name, &self.deny_domains) {
            return Err(format!("domain `{name}` is denied"));
        }
        if !self.allow_domains.is_empty() && !matches_domain(&name, &self.allow_domains) {
            return Err(format!("domain `{name}` is not in the allow list"));
        }
        let private = match host {
            Host::Ipv4(ip) => private_v4(ip),
            Host::Ipv6(ip) => private_v6(ip),
            Host::Domain(_) => {
                name == "localhost"
                    || [".localhost", ".internal", ".local"]
                        .iter()
                        .any(|s| name.ends_with(s))
            }
        };
        if private && !self.allow_private_network {
            return Err(format!("`{name}` is a private/loopback/metadata address"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_dangerous_schemes() {
        let p = UrlPolicy::default();
        for u in [
            "file:///etc/passwd",
            "chrome://settings",
            "javascript:alert(1)",
            "data:text/html,hi",
            "ftp://x.com/",
        ] {
            assert!(p.check_navigation(u).is_err(), "{u}");
        }
        assert!(p.check_navigation("https://example.com/a").is_ok());
        assert!(p.check_navigation("about:blank").is_ok());
        assert!(p.check_request("data:image/png;base64,AA").is_ok());
        assert!(p.check_request("file:///x").is_err());
    }

    #[test]
    fn profile_domains_limit_navigation_but_not_subresources() {
        let p = UrlPolicy {
            document_allow_domains: vec!["example.com".into()],
            ..Default::default()
        };
        assert!(p.check_navigation("https://app.example.com/x").is_ok());
        assert!(p.check_navigation("about:blank").is_ok());
        let e = p.check_navigation("https://evil.io/").unwrap_err();
        assert!(e.contains("profile's domains"), "{e}");
        assert!(p.check_navigation("https://example.com.evil.io/").is_err());
        assert!(p.check_request("https://cdn.other.net/a.js").is_ok());
    }

    #[test]
    fn blocks_private_and_metadata() {
        let p = UrlPolicy::default();
        for u in [
            "http://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:8080/",
            "http://localhost/",
            "http://10.0.0.5/",
            "http://[::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://2130706433/", // decimal 127.0.0.1, normalised by the URL parser
            "http://metadata.google.internal/",
        ] {
            assert!(p.check_navigation(u).is_err(), "{u}");
        }
        let open = UrlPolicy {
            allow_private_network: true,
            ..Default::default()
        };
        assert!(open.check_navigation("http://127.0.0.1:8080/").is_ok());
    }

    #[test]
    fn host_forms_are_normalised_before_comparison() {
        let p = UrlPolicy::default();
        for u in [
            "http://2130706433/",
            "http://0x7f.1/",
            "http://017700000001/",
            "http://0x7f000001/",
            "http://127.1/",
            "http://127.0.0.1./",
            "http://localhost./",
            "http://LOCALHOST/",
            "http://foo.localhost../",
            "http://printer.local./",
            "http://metadata.google.internal./",
            "http://[::ffff:7f00:1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::127.0.0.1]/",
            "http://[64:ff9b::7f00:1]/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[fe80::1]/",
            "http://[fe80::1%25eth0]/", // zone id: rejected outright
            "http://[fe80::1%eth0]/",
            "http://0xA9FEA9FE/", // 169.254.169.254
            "http://0251.0376.0251.0376/",
        ] {
            assert!(p.check_navigation(u).is_err(), "{u}");
            assert!(p.check_request(u).is_err(), "{u}");
        }
        assert!(p.check_navigation("http://93.184.216.34/").is_ok());
        assert!(p.check_navigation("http://example.com./").is_ok());
    }

    #[test]
    fn domain_lists_are_normalised_too() {
        let deny = UrlPolicy {
            deny_domains: vec![
                "Evil.COM.".into(),
                ".b\u{fc}cher.example".into(),
                "0x7f.1".into(),
                "[::1]".into(),
            ],
            allow_private_network: true,
            ..Default::default()
        };
        for u in [
            "https://evil.com/",
            "https://evil.com./",
            "https://EVIL.com/",
            "https://www.evil.com../",
            "https://xn--bcher-kva.example/",
            "https://b\u{fc}cher.example/",
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://[::1]/",
        ] {
            assert!(deny.check_navigation(u).is_err(), "{u}");
        }
        assert!(deny.check_navigation("https://notevil.com/").is_ok());
        let allow = UrlPolicy {
            allow_domains: vec!["Example.COM.".into(), "m\u{fc}nchen.de".into()],
            ..Default::default()
        };
        for u in [
            "https://example.com./",
            "https://a.EXAMPLE.com/",
            "https://xn--mnchen-3ya.de/",
        ] {
            assert!(allow.check_navigation(u).is_ok(), "{u}");
        }
        assert!(allow.check_navigation("https://example.org/").is_err());
    }

    #[test]
    fn network_schemes_other_than_http_are_blocked() {
        let p = UrlPolicy {
            allow_private_network: true,
            ..Default::default()
        };
        for u in [
            "ws://example.com/",
            "wss://example.com/",
            "ftp://example.com/",
            "blob:https://example.com/uuid",
            "ws://127.0.0.1:1/",
        ] {
            assert!(p.check_navigation(u).is_err(), "{u}");
            assert!(p.check_request(u).is_err(), "{u}");
        }
    }

    #[tokio::test]
    async fn dns_names_resolving_to_private_ips_are_blocked() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let p = UrlPolicy {
            resolver: Resolver::new(move |h| match h {
                "meta.nip.test" => Ok(vec![ip("93.184.216.34"), ip("169.254.169.254")]),
                "mapped.test" => Ok(vec![ip("::ffff:10.0.0.1")]),
                "ula.test" => Ok(vec![ip("fd00::1")]),
                "mc.test" => Ok(vec![ip("224.0.0.1")]),
                "ok.test" => Ok(vec![ip("93.184.216.34"), ip("2606:2800:220:1::1")]),
                _ => Err(std::io::Error::other("nxdomain")),
            }),
            ..Default::default()
        };
        for h in [
            "meta.nip.test",
            "mapped.test",
            "ula.test",
            "mc.test",
            "gone.test",
        ] {
            let u = format!("http://{h}/");
            assert!(p.check_resolved(&u, true).await.is_err(), "{h}");
            assert!(p.check_resolved(&u, false).await.is_err(), "{h}");
        }
        assert!(p.check_resolved("http://ok.test/", true).await.is_ok());
        let open = UrlPolicy {
            allow_private_network: true,
            ..p.clone()
        };
        assert!(
            open.check_resolved("http://meta.nip.test/", true)
                .await
                .is_ok()
        );
        assert!(p.remote_ip_denied("[fd00::1]") && p.remote_ip_denied("127.0.0.1"));
        assert!(!p.remote_ip_denied("93.184.216.34") && !open.remote_ip_denied("127.0.0.1"));
    }

    #[test]
    fn transition_addresses_are_classified() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for a in [
            "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo
            "2001::1",
            "2002:0a00:0001::1", // 6to4 of 10.0.0.1
            "2002:7f00:0001::",  // 6to4 of 127.0.0.1
            "2002:a9fe:a9fe::1", // 6to4 of 169.254.169.254
            "64:ff9b::a00:1",    // NAT64 of 10.0.0.1
        ] {
            assert!(ip_is_private(ip(a)), "{a}");
        }
        for a in [
            "2002:5db8:d822::1",
            "2001:db8::1",
            "2001:4860:4860::8888",
            "64:ff9b::5db8:d822",
        ] {
            assert!(!ip_is_private(ip(a)), "{a}");
        }
        assert!(
            UrlPolicy::default()
                .check_navigation("http://[2001:0:4136:e378:8000:63bf:3fff:fdd2]/")
                .is_err()
        );
    }

    /// Slow resolver that records calls and peak concurrency.
    fn slow(
        ms: u64,
    ) -> (
        Resolver,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let (calls, now, peak) = (
            Arc::new(AtomicUsize::new(0)),
            AtomicUsize::new(0),
            Arc::new(AtomicUsize::new(0)),
        );
        let (c, p) = (calls.clone(), peak.clone());
        let r = Resolver::new(move |h| {
            c.fetch_add(1, SeqCst);
            p.fetch_max(now.fetch_add(1, SeqCst) + 1, SeqCst);
            std::thread::sleep(Duration::from_millis(ms));
            now.fetch_sub(1, SeqCst);
            if h.starts_with("bad") {
                Err(std::io::Error::other("nxdomain"))
            } else {
                Ok(vec!["93.184.216.34".parse().unwrap()])
            }
        });
        (r, calls, peak)
    }

    #[tokio::test]
    async fn concurrent_lookups_of_one_host_run_once() {
        use std::sync::atomic::Ordering::SeqCst;
        let (r, calls, _) = slow(200);
        let hs: Vec<_> = (0..20)
            .map(|_| {
                let r = r.clone();
                tokio::spawn(async move { r.resolve("same.test", false).await })
            })
            .collect();
        for h in hs {
            assert!(h.await.unwrap().is_ok());
        }
        assert_eq!(calls.load(SeqCst), 1);
        assert!(r.gates.lock().unwrap().is_empty(), "gates are released");
    }

    #[tokio::test]
    async fn lookups_in_flight_are_limited() {
        use std::sync::atomic::Ordering::SeqCst;
        let (r, calls, peak) = slow(150);
        let hs: Vec<_> = (0..32)
            .map(|i| {
                let r = r.clone();
                tokio::spawn(async move { r.resolve(&format!("h{i}.test"), false).await })
            })
            .collect();
        for h in hs {
            assert!(h.await.unwrap().is_ok());
        }
        assert_eq!(calls.load(SeqCst), 32);
        assert!(
            peak.load(SeqCst) <= MAX_IN_FLIGHT,
            "peak {}",
            peak.load(SeqCst)
        );
    }

    #[tokio::test]
    async fn failures_are_cached_briefly() {
        use std::sync::atomic::Ordering::SeqCst;
        let (r, calls, _) = slow(1);
        assert!(r.resolve("bad.test", false).await.is_err());
        assert!(r.resolve("bad.test", false).await.is_err());
        assert_eq!(calls.load(SeqCst), 1);
        // expired failure entries are looked up again
        r.cache.lock().unwrap().get_mut("bad.test").unwrap().0 =
            Instant::now() - FAILURE_TTL - Duration::from_millis(1);
        assert!(r.resolve("bad.test", false).await.is_err());
        assert_eq!(calls.load(SeqCst), 2);
    }

    /// Resolver that sleeps `ms` for hosts starting with "slow" and answers at once otherwise.
    fn mixed(ms: u64) -> Resolver {
        Resolver::new(move |h| {
            if h.starts_with("slow") {
                std::thread::sleep(Duration::from_millis(ms));
            }
            Ok(vec!["93.184.216.34".parse().unwrap()])
        })
    }

    #[tokio::test]
    async fn cancelled_resolve_leaves_no_gate() {
        let r = mixed(300);
        let t = tokio::time::timeout(Duration::from_millis(50), r.resolve("slow.test", false));
        assert!(t.await.is_err());
        assert!(r.gates.lock().unwrap().is_empty(), "gate leaked on cancel");
        // a waiter cancelled behind a holder does not leak either
        let h = tokio::spawn({
            let r = r.clone();
            async move { r.resolve("slow2.test", false).await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let w = tokio::time::timeout(Duration::from_millis(30), r.resolve("slow2.test", false));
        assert!(w.await.is_err());
        assert!(h.await.unwrap().is_ok());
        assert!(r.gates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hostile_lookups_cannot_starve_priority_navigation() {
        let r = mixed(3_000);
        for i in 0..20 {
            let r = r.clone();
            tokio::spawn(async move { r.resolve(&format!("slow{i}.test"), false).await });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        // all general permits are held by slow lookups; a top-level host still resolves
        let ok =
            tokio::time::timeout(Duration::from_millis(500), r.resolve("nav.test", true)).await;
        assert!(ok.is_ok_and(|r| r.is_ok()));
        let starved =
            tokio::time::timeout(Duration::from_millis(500), r.resolve("sub.test", false));
        assert!(starved.await.is_err(), "general pool should be exhausted");
    }

    #[tokio::test]
    async fn timeouts_are_not_cached_as_failures() {
        let r = mixed(3_000);
        let e = r.resolve("slow.test", false).await.unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        assert!(r.cache.lock().unwrap().is_empty(), "timeout was cached");
        assert!(r.gates.lock().unwrap().is_empty());
    }

    #[test]
    fn domain_lists() {
        let p = UrlPolicy {
            allow_domains: vec!["example.com".into()],
            deny_domains: vec!["evil.example.com".into()],
            ..Default::default()
        };
        assert!(p.check_navigation("https://example.com/").is_ok());
        assert!(p.check_navigation("https://a.example.com/").is_ok());
        assert!(p.check_navigation("https://evil.example.com/").is_err());
        assert!(p.check_navigation("https://notexample.com/").is_err());
    }
}
