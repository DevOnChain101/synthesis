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
use std::net::{IpAddr, ToSocketAddrs};
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

pub type Grants = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Per tab (CDP session) bookkeeping of the person's pages.
#[derive(Debug, Default)]
pub struct Pages {
    /// Loopback targets of navigations the person started and that have not committed yet.
    pending: std::collections::HashMap<String, Vec<String>>,
    /// Tabs whose top-level page is a loopback target the person opened.
    top: std::collections::HashSet<String>,
    /// Chrome reports one page through several CDP sessions (ours and the browser-level auto-attach one): all
    /// map to the page target, which is what the two sets above are keyed by.
    target_of: std::collections::HashMap<String, String>,
}

impl Pages {
    fn key(&self, session: &str) -> String {
        self.target_of
            .get(session)
            .cloned()
            .unwrap_or_else(|| session.to_owned())
    }
}

/// Whose request the network hook is judging; decides which loopback grants count.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// The agent's (or a public page's): only targets approved for the agent.
    Agent,
    /// A navigation the person is making right now: also that one `host:port`, nothing else.
    Pending(String),
    /// A request of a page the person opened on loopback: also every target the person opened.
    Person,
}

/// A person navigation in flight on one tab; dropping it (load done or failed) ends it.
pub struct PersonNav {
    pages: std::sync::Arc<std::sync::Mutex<Pages>>,
    session: String,
    target: String,
}

impl Drop for PersonNav {
    fn drop(&mut self) {
        let mut p = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = p.pending.get_mut(&self.session) {
            if let Some(i) = v.iter().position(|t| *t == self.target) {
                v.remove(i);
            }
            if v.is_empty() {
                p.pending.remove(&self.session);
            }
        }
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
    /// Loopback (localhost, 127.0.0.0/8, ::1) is reachable ONLY on these ports, while the rest of
    /// the private network stays blocked: a login on `http://localhost:3000` opens port 3000.
    pub loopback_ports: Vec<u16>,
    /// Loopback `host:port` targets (see `loopback_target`) the PERSON opened by typing an address, recorded when the
    /// navigation commits. Shared by every clone, so the network hooks of the running browser see them. They apply
    /// only to the person's own navigation and to the requests of a page the person opened (`Origin`); the agent's
    /// navigation, and any page it drives, never use them.
    pub user_loopback: Grants,
    /// Which tabs hold a person navigation or a person-opened loopback page.
    pub pages: std::sync::Arc<std::sync::Mutex<Pages>>,
    /// Loopback targets the person APPROVED for the agent (`approve_loopback`), one exact `host:port` each.
    pub agent_loopback: Grants,
    /// Used by `check_resolved` to look hostnames up.
    pub resolver: Resolver,
}

pub use synthesis_platform::net::ip_blocked as ip_is_private;

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

/// `host:port` of an http(s) URL whose host is loopback (`localhost`, 127.0.0.0/8, ::1), host normalised: the unit
/// of every loopback approval.
pub fn loopback_target(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let host = match u.host()? {
        Host::Domain(d) if normalise(d) == "localhost" => "localhost".to_owned(),
        Host::Ipv4(ip) if ip.is_loopback() => ip.to_string(),
        Host::Ipv6(ip) if ip.is_loopback() => format!("[{ip}]"),
        _ => return None,
    };
    matches!(u.scheme(), "http" | "https").then_some(())?;
    Some(format!("{host}:{}", u.port_or_known_default()?))
}

fn loopback_port(url: &str) -> Option<u16> {
    loopback_target(url)?.rsplit(':').next()?.parse().ok()
}

impl UrlPolicy {
    /// Top-level (and frame) navigation by the AGENT: only http(s) and `about:blank`. Loopback only on a target
    /// the person approved for the agent; never on one the person merely opened.
    pub fn check_navigation(&self, raw: &str) -> Result<(), String> {
        self.check_nav(raw, &Origin::Agent)
    }

    /// A navigation the PERSON typed or reloaded: like `check_navigation`, except that a loopback host (localhost,
    /// 127.0.0.0/8, ::1) is allowed on exactly that `host:port`. Changes nothing; the port is recorded when the
    /// navigation commits (`committed`), and only the hook of that one navigation (`begin_person`) honours it before.
    /// Everything else private (RFC1918, link-local, metadata, other loopback ports, names that resolve there) stays
    /// refused, and the deny/allow lists still apply.
    pub fn check_navigation_person(&self, raw: &str) -> Result<(), String> {
        let o = loopback_target(raw).map_or(Origin::Agent, Origin::Pending);
        self.check_nav(raw, &o)
    }

    /// The person approved the AGENT browsing this exact loopback `host:port`. Checked like a navigation first
    /// (scheme, deny/allow lists), so a refused URL leaves no grant.
    pub fn approve_loopback(&self, raw: &str) -> Result<(), String> {
        let target = self.check_navigation_if_approved(raw)?;
        let mut open = self
            .agent_loopback
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !open.contains(&target) {
            open.push(target);
        }
        Ok(())
    }

    /// `check_navigation` on a copy where the person had approved this loopback target; returns the target. Fails for
    /// any other reason (deny lists, profile domains, not loopback), so nobody is asked in vain.
    pub fn check_navigation_if_approved(&self, raw: &str) -> Result<String, String> {
        let target = loopback_target(raw).ok_or("not a loopback address")?;
        let mut with = self.clone();
        with.agent_loopback = std::sync::Arc::new(std::sync::Mutex::new(vec![target.clone()]));
        with.check_nav(raw, &Origin::Agent)?;
        Ok(target)
    }

    /// The agent can read or act on a page at `url` (not loopback, or a target approved for it).
    pub fn agent_may_use_page(&self, url: &str) -> bool {
        loopback_target(url).is_none() || self.check_navigation(url).is_ok()
    }

    fn pages(&self) -> std::sync::MutexGuard<'_, Pages> {
        self.pages.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The person starts a navigation to `url` in tab `session`: its document requests may reach that loopback
    /// target until the guard drops. `None` for a URL that is not loopback.
    pub fn begin_person(&self, session: &str, url: &str) -> Option<PersonNav> {
        let target = loopback_target(url)?;
        let mut p = self.pages();
        let key = p.key(session);
        p.pending
            .entry(key.clone())
            .or_default()
            .push(target.clone());
        Some(PersonNav {
            pages: self.pages.clone(),
            session: key,
            target,
        })
    }

    /// The agent navigates tab `session` away: it is no longer the person's page.
    pub fn leave_person_page(&self, session: &str) {
        let mut p = self.pages();
        let k = p.key(session);
        p.top.remove(&k);
    }

    /// CDP session `session` shows page target `target`.
    pub fn alias(&self, session: &str, target: &str) {
        self.pages()
            .target_of
            .insert(session.to_owned(), target.to_owned());
    }

    /// The top-level page of tab `session` committed at `url`. A loopback page the person's navigation was heading
    /// to becomes the person's page and its port is recorded; any other page is not.
    pub fn committed(&self, session: &str, url: &str) {
        let target = loopback_target(url);
        let mut pages = self.pages();
        let session = &pages.key(session);
        let person = target.as_ref().is_some_and(|t| {
            pages.pending.get(session).is_some_and(|v| v.contains(t))
                || pages.top.contains(session)
                    && self
                        .user_loopback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(t)
        });
        if person {
            pages.top.insert(session.to_owned());
            let t = target.expect("person implies a target");
            let mut open = self.user_loopback.lock().unwrap_or_else(|e| e.into_inner());
            if !open.contains(&t) {
                open.push(t);
            }
        } else {
            pages.top.remove(session);
        }
    }

    /// Whose request this is: a navigation the person is making, a page the person opened, or the agent's.
    pub fn origin_for(&self, session: Option<&str>, document: bool) -> Origin {
        let Some(s) = session else {
            return Origin::Agent;
        };
        let pages = self.pages();
        let s = &pages.key(s);
        if document && let Some(t) = pages.pending.get(s).and_then(|v| v.last()) {
            return Origin::Pending(t.clone());
        }
        if pages.top.contains(s) {
            Origin::Person
        } else {
            Origin::Agent
        }
    }

    /// Every grant starts empty: grants belong to one browser (one session), not to the settings it was built from.
    pub(crate) fn reset_grants(&mut self) {
        self.user_loopback = Grants::default();
        self.agent_loopback = Grants::default();
        self.pages = Default::default();
    }

    fn check_nav(&self, raw: &str, o: &Origin) -> Result<(), String> {
        if raw == "about:blank" {
            return Ok(());
        }
        self.check(raw, &["http", "https"], o)?;
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
    pub fn check_request(&self, raw: &str, o: &Origin) -> Result<(), String> {
        // ws(s), ftp and blob are never allowed: WebSocket handshakes bypass Fetch interception.
        self.check(raw, &["http", "https", "data", "about"], o)
    }

    /// `check_navigation` / `check_request` plus DNS: for a hostname, every address it resolves
    /// to must be public (unless `allow_private_network`). Fails closed when it cannot resolve.
    pub async fn check_resolved(
        &self,
        raw: &str,
        document: bool,
        o: &Origin,
    ) -> Result<(), String> {
        if document {
            self.check_nav(raw, o)?;
        } else {
            self.check_request(raw, o)?;
        }
        if self.allow_private_network {
            return Ok(());
        }
        if let Some(Host::Domain(d)) = Url::parse(raw).map_err(|e| e.to_string())?.host() {
            let name = normalise(d);
            if name == "localhost" && self.loopback_port_ok(raw, o) {
                return Ok(()); // `check` already vetted the port; localhost resolves to loopback
            }
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
    pub fn remote_ip_denied(&self, remote: &str, url: &str, o: &Origin) -> bool {
        !self.allow_private_network
            && remote
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| {
                    ip_is_private(ip) && !(ip.is_loopback() && self.loopback_port_ok(url, o))
                })
    }

    /// `url` is a loopback host on one of `loopback_ports`, or a target approved for the agent, or one the person
    /// opened that `o` may reach.
    fn loopback_port_ok(&self, url: &str, o: &Origin) -> bool {
        let Some(t) = loopback_target(url) else {
            return false;
        };
        let has = |g: &Grants| g.lock().unwrap_or_else(|e| e.into_inner()).contains(&t);
        loopback_port(url).is_some_and(|p| self.loopback_ports.contains(&p))
            || has(&self.agent_loopback)
            || match o {
                Origin::Agent => false,
                Origin::Pending(p) => *p == t,
                Origin::Person => has(&self.user_loopback),
            }
    }

    fn check(&self, raw: &str, schemes: &[&str], o: &Origin) -> Result<(), String> {
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
            Host::Ipv4(ip) => ip_is_private(ip.into()),
            Host::Ipv6(ip) => ip_is_private(ip.into()),
            Host::Domain(_) => {
                name == "localhost"
                    || [".localhost", ".internal", ".local"]
                        .iter()
                        .any(|s| name.ends_with(s))
            }
        };
        if private && !self.allow_private_network && !self.loopback_port_ok(raw, o) {
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
        assert!(
            p.check_request("data:image/png;base64,AA", &Origin::Agent)
                .is_ok()
        );
        assert!(p.check_request("file:///x", &Origin::Agent).is_err());
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
        assert!(
            p.check_request("https://cdn.other.net/a.js", &Origin::Agent)
                .is_ok()
        );
    }

    #[test]
    fn loopback_gate_is_scoped_to_the_recorded_port() {
        let p = UrlPolicy {
            loopback_ports: vec![3000],
            ..Default::default()
        };
        for ok in [
            "http://localhost:3000/a",
            "http://127.0.0.1:3000/",
            "http://[::1]:3000/",
        ] {
            assert!(p.check_navigation(ok).is_ok(), "{ok}");
            assert!(p.check_request(ok, &Origin::Agent).is_ok(), "{ok}");
        }
        for bad in [
            "http://localhost:3001/",
            "http://127.0.0.1/",
            "http://10.0.0.5:3000/",
            "http://169.254.169.254:3000/",
            "http://metadata.google.internal:3000/",
        ] {
            assert!(p.check_navigation(bad).is_err(), "{bad}");
        }
        assert!(!p.remote_ip_denied("127.0.0.1", "http://localhost:3000/x", &Origin::Agent));
        assert!(p.remote_ip_denied("127.0.0.1", "http://localhost:3001/x", &Origin::Agent));
        assert!(p.remote_ip_denied(
            "127.0.0.1",
            "http://rebind.example.com:3000/",
            &Origin::Agent
        ));
        assert!(p.remote_ip_denied("10.0.0.5", "http://localhost:3000/", &Origin::Agent));
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
            assert!(p.check_request(u, &Origin::Agent).is_err(), "{u}");
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
            assert!(p.check_request(u, &Origin::Agent).is_err(), "{u}");
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
            assert!(
                p.check_resolved(&u, true, &Origin::Agent).await.is_err(),
                "{h}"
            );
            assert!(
                p.check_resolved(&u, false, &Origin::Agent).await.is_err(),
                "{h}"
            );
        }
        assert!(
            p.check_resolved("http://ok.test/", true, &Origin::Agent)
                .await
                .is_ok()
        );
        let open = UrlPolicy {
            allow_private_network: true,
            ..p.clone()
        };
        assert!(
            open.check_resolved("http://meta.nip.test/", true, &Origin::Agent)
                .await
                .is_ok()
        );
        assert!(
            p.remote_ip_denied("[fd00::1]", "http://x/", &Origin::Agent)
                && p.remote_ip_denied("127.0.0.1", "http://x/", &Origin::Agent)
        );
        assert!(
            !p.remote_ip_denied("93.184.216.34", "http://x/", &Origin::Agent)
                && !open.remote_ip_denied("127.0.0.1", "http://x/", &Origin::Agent)
        );
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
            "2001:db8::1",       // documentation range
        ] {
            assert!(ip_is_private(ip(a)), "{a}");
        }
        for a in [
            "2002:5db8:d822::1",
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

    fn person_page(p: &UrlPolicy, session: &str, url: &str) {
        let _nav = p.begin_person(session, url);
        p.committed(session, url);
    }

    #[test]
    fn the_person_may_type_loopback_the_agent_may_not() {
        let p = UrlPolicy::default();
        for u in [
            "http://localhost:3000/",
            "http://127.0.0.1:3000",
            "http://[::1]:3000/",
        ] {
            assert!(p.check_navigation(u).is_err(), "agent {u}");
            assert!(p.check_navigation_person(u).is_ok(), "person {u}");
        }
        // nothing is granted until the navigation commits
        assert!(p.user_loopback.lock().unwrap().is_empty());
        person_page(&p, "tab1", "http://localhost:3000/");
        assert_eq!(*p.user_loopback.lock().unwrap(), ["localhost:3000"]);
        // the person's grant never opens the agent's navigation, even to the same target
        assert!(p.check_navigation("http://localhost:3000/").is_err());
        // a fresh policy (a new browser) starts empty
        let mut fresh = p.clone();
        fresh.reset_grants();
        assert!(fresh.user_loopback.lock().unwrap().is_empty());
    }

    #[test]
    fn a_page_of_the_person_reaches_only_what_the_person_opened() {
        let p = UrlPolicy::default();
        let (agent, person) = (Origin::Agent, Origin::Person);
        person_page(&p, "tab1", "http://localhost:3000/");
        assert_eq!(p.origin_for(Some("tab1"), false), Origin::Person);
        assert_eq!(p.origin_for(Some("tab1"), true), Origin::Person);
        assert_eq!(p.origin_for(Some("tab2"), true), Origin::Agent);
        assert!(
            p.check_request("http://localhost:3000/app.js", &person)
                .is_ok()
        );
        assert!(p.check_request("http://localhost:3001/", &person).is_err());
        assert!(p.check_request("http://127.0.0.2:3000/", &person).is_err());
        assert!(p.check_request("http://10.0.0.1/", &person).is_err());
        assert!(p.check_request("http://169.254.169.254/", &person).is_err());
        // the agent's hooks never see the person's grant
        assert!(
            p.check_request("http://localhost:3000/app.js", &agent)
                .is_err()
        );
        // the agent navigates the tab away: it is not the person's page any more
        p.leave_person_page("tab1");
        assert_eq!(p.origin_for(Some("tab1"), true), Origin::Agent);
    }

    #[tokio::test]
    async fn an_agent_document_or_redirect_to_a_person_grant_is_refused() {
        let p = UrlPolicy::default();
        person_page(&p, "tab1", "http://localhost:3000/");
        // a public page the agent drives (tab2) redirects, frames or navigates to the person's port
        let o = p.origin_for(Some("tab2"), true);
        assert_eq!(o, Origin::Agent);
        for document in [true, false] {
            assert!(
                p.check_resolved("http://localhost:3000/", document, &o)
                    .await
                    .is_err()
            );
        }
        // the agent approved :4000; its page redirects to the person's :3000 (an agent-origin hop): refused
        p.approve_loopback("http://localhost:4000/").unwrap();
        assert!(
            p.check_resolved("http://localhost:4000/", true, &o)
                .await
                .is_ok()
        );
        assert!(
            p.check_resolved("http://localhost:3000/", true, &o)
                .await
                .is_err()
        );
        // the person's own navigation to :3000 passes only for that port, only while it is in flight
        let nav = p.begin_person("tab3", "http://localhost:3000/").unwrap();
        let o = p.origin_for(Some("tab3"), true);
        assert!(
            p.check_resolved("http://localhost:3000/", true, &o)
                .await
                .is_ok()
        );
        assert!(
            p.check_resolved("http://localhost:3001/", true, &o)
                .await
                .is_err()
        );
        assert!(
            p.check_resolved("http://10.0.0.1/", true, &o)
                .await
                .is_err()
        );
        drop(nav);
        assert_eq!(p.origin_for(Some("tab3"), true), Origin::Agent);
    }

    #[test]
    fn private_link_local_and_metadata_stay_refused_for_the_person() {
        let p = UrlPolicy::default();
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://[fd00:ec2::254]/",
            "http://10.0.0.1:3000/",
            "http://192.168.1.1/",
            "http://172.16.0.1/",
            "http://metadata.google.internal/",
            "file:///etc/passwd",
        ] {
            assert!(p.check_navigation_person(u).is_err(), "{u}");
            assert!(p.approve_loopback(u).is_err(), "{u}");
            assert!(p.check_request(u, &Origin::Person).is_err(), "{u}");
        }
    }

    #[test]
    fn an_approved_loopback_target_is_exact_and_only_for_that_target() {
        let p = UrlPolicy::default();
        let a = Origin::Agent;
        assert!(p.check_navigation("http://localhost:3000/").is_err());
        p.approve_loopback("http://localhost:3000/").unwrap();
        assert!(p.check_navigation("http://localhost:3000/x").is_ok());
        assert!(p.agent_may_use_page("http://localhost:3000/x"));
        assert!(!p.agent_may_use_page("http://localhost:3001/"));
        assert!(p.agent_may_use_page("https://example.com/"));
        assert!(p.check_navigation("http://localhost:3001/").is_err());
        assert!(p.check_navigation("http://127.0.0.1:3000/").is_err());
        // approval is for loopback only; LAN and metadata have no approval path
        assert!(p.approve_loopback("http://192.168.1.1:3000/").is_err());
        assert!(p.check_navigation("http://192.168.1.1:3000/").is_err());
        // a redirect or subresource from the approved page
        assert!(p.check_request("http://10.0.0.1/", &a).is_err());
        assert!(p.check_request("http://169.254.169.254/", &a).is_err());
        assert!(p.check_request("http://localhost:3001/", &a).is_err());
        // the deny list still wins, and a refused URL leaves no grant
        let deny = UrlPolicy {
            deny_domains: vec!["localhost".into()],
            ..UrlPolicy::default()
        };
        assert!(deny.approve_loopback("http://localhost:3000/").is_err());
        assert!(deny.check_request("http://localhost:3000/", &a).is_err());
    }

    #[tokio::test]
    async fn names_that_resolve_to_loopback_or_private_are_refused_even_after_a_grant() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let p = UrlPolicy {
            resolver: Resolver::new(move |h| match h {
                "rebind.test" => Ok(vec![ip("127.0.0.1")]),
                "lan.test" => Ok(vec![ip("10.0.0.5")]),
                _ => Err(std::io::Error::other("nxdomain")),
            }),
            ..Default::default()
        };
        person_page(&p, "tab1", "http://localhost:3000/");
        p.approve_loopback("http://localhost:3000/").unwrap();
        let o = Origin::Person;
        for u in ["http://rebind.test:3000/", "http://lan.test:3000/"] {
            assert!(p.check_resolved(u, true, &o).await.is_err(), "{u}");
            assert!(p.check_resolved(u, false, &o).await.is_err(), "{u}");
        }
        assert!(
            p.check_resolved("http://localhost:3000/", true, &o)
                .await
                .is_ok()
        );
        assert!(
            p.check_resolved("http://localhost:3001/", true, &o)
                .await
                .is_err()
        );
        // Chrome connected to loopback for a name that is not the granted target: dropped
        assert!(p.remote_ip_denied("127.0.0.1", "http://rebind.test:3000/", &o));
        assert!(!p.remote_ip_denied("127.0.0.1", "http://localhost:3000/", &o));
        assert!(p.remote_ip_denied("10.0.0.5", "http://localhost:3000/", &o));
    }
}
