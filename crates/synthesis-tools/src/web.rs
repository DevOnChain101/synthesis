//! WebFetch and WebSearch (ports of webfetch.js / websearch.js). Everything fetched is data:
//! the text is returned inside an `UntrustedText` wrapper.
//!
//! SSRF defence: only http(s); the URL policy (`WebConfig`) is checked on the first URL and on
//! every redirect hop (redirects are followed here, never by reqwest); hostnames are resolved by
//! OUR resolver, which validates every returned address and hands only those to the connector, so
//! a DNS answer cannot change between check and connect.
//! shortcut: with a proxy configured the proxy resolves the target, so only the URL-level checks
//! (scheme, literal IPs, `localhost`-style names, domain lists) apply to proxied requests; the
//! egress proxy is then the enforcement point. Upgrade by resolving+pinning before the proxy hop.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::{Captures, Regex};
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::{Value, json};
use synthesis_browser::{UntrustedText, UrlPolicy};
use synthesis_config::Env;
use synthesis_core::ToolContext;
use synthesis_platform::net::ip_blocked;

use crate::native::{Shared, lock};
use crate::util::str_field;
use crate::{BoxFuture, ToolOutput};

const MAX_BYTES: usize = 5 * 1024 * 1024;
const MAX_CHARS: usize = 50_000;
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 10;
const SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
const SEARCH_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 10;

/// Settings of WebFetch / WebSearch (set with `Registry::with_web`). Default: deny private,
/// loopback, link-local and metadata addresses.
#[derive(Debug, Clone, Default)]
pub struct WebConfig {
    pub policy: UrlPolicy,
}

type Lookup = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
        + Send
        + Sync,
>;

fn system_lookup() -> Lookup {
    Arc::new(|host| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    })
}

struct Guard {
    lookup: Lookup,
    allow_private: bool,
    blocked: fn(IpAddr) -> bool,
    /// Proxy hostnames: the connector must be able to reach them.
    exempt: Vec<String>,
}

impl Guard {
    async fn validated(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let ips = (self.lookup)(host.to_string())
            .await
            .map_err(|e| format!("cannot resolve {host}: {e}"))?;
        if ips.is_empty() {
            return Err(format!("cannot resolve {host}: no addresses"));
        }
        if !self.allow_private
            && !self.exempt.iter().any(|e| e == host)
            && let Some(ip) = ips.iter().find(|ip| (self.blocked)(**ip))
        {
            return Err(format!(
                "{host} resolves to a private/loopback/metadata address ({ip})"
            ));
        }
        Ok(ips)
    }
}

struct GuardResolver(Arc<Guard>);

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let g = self.0.clone();
        let host = name.as_str().to_ascii_lowercase();
        Box::pin(async move {
            let ips = g.validated(&host).await?;
            Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

fn var<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
    env.get_nonempty(name)
        .or_else(|| env.get_nonempty(&name.to_lowercase()))
}

/// Proxy and CA setup per the standard variables, read from `env` only (same rules as
/// synthesis-providers `build_client`, which cannot take a custom resolver).
fn builder(env: &Env) -> Result<(reqwest::ClientBuilder, Vec<String>), String> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (compatible; Synthesis)")
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    let no_proxy = var(env, "NO_PROXY").and_then(reqwest::NoProxy::from_string);
    let mut hosts = vec![];
    for name in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        if let Some(url) = var(env, name) {
            let p = match name {
                "HTTPS_PROXY" => reqwest::Proxy::https(url),
                "HTTP_PROXY" => reqwest::Proxy::http(url),
                _ => reqwest::Proxy::all(url),
            }
            .map_err(|_| format!("invalid proxy URL in {name}"))?; // never echo: may hold credentials
            if let Some(h) = Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
            {
                hosts.push(h);
            }
            b = b.proxy(p.no_proxy(no_proxy.clone()));
        }
    }
    let mut files: Vec<std::path::PathBuf> = vec![];
    if let Some(f) = env.get_nonempty("SSL_CERT_FILE") {
        files.push(f.into());
    }
    if let Some(d) = env.get_nonempty("SSL_CERT_DIR") {
        for dir in std::env::split_paths(d) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                files.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_file()));
            }
        }
    }
    for pem in files.iter().filter_map(|f| std::fs::read(f).ok()) {
        for c in reqwest::Certificate::from_pem_bundle(&pem)
            .into_iter()
            .flatten()
        {
            b = b.add_root_certificate(c);
        }
    }
    Ok((b, hosts))
}

pub(crate) struct Fetched {
    pub url: Url,
    pub status: reqwest::StatusCode,
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
}

pub(crate) struct Fetcher {
    client: reqwest::Client,
    policy: UrlPolicy,
    blocked: fn(IpAddr) -> bool,
    proxy_hosts: Vec<String>,
}

fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s += &format!(": {c}");
        src = c.source();
    }
    s
}

impl Fetcher {
    pub(crate) fn new(env: &Env, policy: UrlPolicy) -> Result<Self, String> {
        Self::with(env, policy, system_lookup(), ip_blocked)
    }

    /// `lookup` and `blocked` are injectable so tests can simulate DNS answers and reach a
    /// loopback fixture while every other address stays blocked.
    pub(crate) fn with(
        env: &Env,
        policy: UrlPolicy,
        lookup: Lookup,
        blocked: fn(IpAddr) -> bool,
    ) -> Result<Self, String> {
        let (b, proxy_hosts) = builder(env)?;
        let guard = Guard {
            lookup,
            allow_private: policy.allow_private_network,
            blocked,
            exempt: proxy_hosts.clone(),
        };
        let client = b
            .dns_resolver(Arc::new(GuardResolver(Arc::new(guard))))
            .build()
            .map_err(|e| format!("cannot build HTTP client: {}", chain(&e)))?;
        Ok(Self {
            client,
            policy,
            blocked,
            proxy_hosts,
        })
    }

    fn check_url(&self, u: &Url) -> Result<(), String> {
        if !matches!(u.scheme(), "http" | "https") {
            return Err(format!("scheme `{}` is not allowed", u.scheme()));
        }
        if !u.username().is_empty() || u.password().is_some() {
            return Err("URLs with credentials are not allowed".into());
        }
        self.policy.check_navigation(u.as_str())?;
        let private_ok = self.policy.allow_private_network;
        match u.host() {
            Some(url::Host::Ipv4(ip)) if !private_ok && (self.blocked)(ip.into()) => {
                Err(format!("`{ip}` is a private/loopback/metadata address"))
            }
            Some(url::Host::Ipv6(ip)) if !private_ok && (self.blocked)(ip.into()) => {
                Err(format!("`{ip}` is a private/loopback/metadata address"))
            }
            // The proxy's own name is exempt in the resolver; it must not be a fetch target.
            Some(url::Host::Domain(d))
                if !private_ok && self.proxy_hosts.contains(&d.to_ascii_lowercase()) =>
            {
                Err(format!("`{d}` is the configured proxy"))
            }
            Some(_) => Ok(()),
            None => Err("URL has no host".into()),
        }
    }

    /// GET with manual redirects (each hop re-checked), a streaming size cap and an overall
    /// timeout. The body is cut at `max_bytes` and the connection dropped.
    pub(crate) async fn get(
        &self,
        url: Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Fetched, String> {
        tokio::time::timeout(timeout, self.get_inner(url, max_bytes))
            .await
            .map_err(|_| "timed out".to_string())?
    }

    async fn get_inner(&self, mut url: Url, max_bytes: usize) -> Result<Fetched, String> {
        for _ in 0..=MAX_REDIRECTS {
            self.check_url(&url)?;
            let mut resp = self
                .client
                .get(url.clone())
                .send()
                .await
                .map_err(|e| chain(&e))?;
            let status = resp.status();
            if status.is_redirection()
                && let Some(loc) = resp.headers().get(reqwest::header::LOCATION)
            {
                let loc = loc.to_str().map_err(|_| "invalid Location header")?;
                url = url
                    .join(loc)
                    .map_err(|e| format!("invalid redirect: {e}"))?;
                continue;
            }
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let mut body = Vec::new();
            let mut truncated = false;
            while let Some(c) = resp.chunk().await.map_err(|e| chain(&e))? {
                if body.len() + c.len() > max_bytes {
                    body.extend_from_slice(&c[..max_bytes - body.len()]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(&c);
            }
            return Ok(Fetched {
                url,
                status,
                content_type,
                body,
                truncated,
            });
        }
        Err(format!("too many redirects (more than {MAX_REDIRECTS})"))
    }
}

static SCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<script.*?</script>").unwrap());
static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<style.*?</style>").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#[xX][0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static LINE_EDGES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]*\n[ \t]*").unwrap());
static BLANKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

fn decode_entities(text: &str) -> String {
    ENTITY
        .replace_all(text, |c: &Captures| {
            let code = &c[1];
            let cp = match code.as_bytes()[0] {
                b'#' => match code.as_bytes().get(1) {
                    Some(b'x' | b'X') => u32::from_str_radix(&code[2..], 16).ok(),
                    _ => code[1..].parse().ok(),
                },
                _ => {
                    return match code {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        "nbsp" => " ",
                        _ => return c[0].to_string(),
                    }
                    .to_string();
                }
            };
            cp.and_then(char::from_u32)
                .map_or_else(|| c[0].to_string(), String::from)
        })
        .into_owned()
}

/// Port of webfetch.js `htmlToText`: drop script/style/comments/tags, decode entities, tidy blanks.
pub(crate) fn html_to_text(html: &str) -> String {
    let t = SCRIPT.replace_all(html, " ");
    let t = STYLE.replace_all(&t, " ");
    let t = COMMENT.replace_all(&t, " ");
    let t = TAG.replace_all(&t, " ");
    let t = decode_entities(&t);
    let t = SPACES.replace_all(&t, " ");
    let t = LINE_EDGES.replace_all(&t, "\n");
    BLANKS.replace_all(&t, "\n\n").trim().to_string()
}

/// Charset from the Content-Type header, else a BOM or `<meta charset>` in the first 1 KiB, else UTF-8.
fn decode_body(body: &[u8], content_type: &str) -> String {
    static META: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)charset\s*=\s*["']?([a-z0-9_:.-]+)"#).unwrap());
    let from_header = content_type
        .split(';')
        .skip(1)
        .find_map(|p| {
            p.trim()
                .strip_prefix("charset=")
                .or_else(|| p.trim().strip_prefix("Charset="))
        })
        .map(|v| v.trim_matches('"').to_string());
    let label = from_header.or_else(|| {
        let head = String::from_utf8_lossy(&body[..body.len().min(1024)]);
        META.captures(&head).map(|c| c[1].to_string())
    });
    let enc = label
        .and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    enc.decode(body).0.into_owned()
}

fn textual(content_type: &str) -> bool {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ct.is_empty()
        || ct.starts_with("text/")
        || [
            "json",
            "xml",
            "javascript",
            "yaml",
            "x-www-form-urlencoded",
            "markdown",
            "toml",
        ]
        .iter()
        .any(|k| ct.contains(k))
}

fn untrusted(s: String) -> String {
    UntrustedText::new(s).to_string()
}

fn truncate_chars(mut text: String) -> String {
    if let Some((i, _)) = text.char_indices().nth(MAX_CHARS) {
        text.truncate(i);
        text += "\n\n… [truncated]";
    }
    text
}

fn err(kind: &str, msg: String) -> ToolOutput {
    ToolOutput::error(msg).with_meta(json!({"error": kind}))
}

fn kind_of(msg: &str) -> &'static str {
    if msg == "timed out" {
        "timeout"
    } else if msg.contains("private/loopback")
        || msg.contains("not allowed")
        || msg.contains("allow list")
        || msg.contains("is denied")
        || msg.contains("configured proxy")
    {
        "url_policy"
    } else {
        "fetch_error"
    }
}

fn env_of(ctx: &ToolContext) -> Env {
    Env::from_pairs(ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())))
}

/// `http://` becomes `https://` (the tool description promises it), except for a private host the
/// user explicitly opted into: a local dev server has no TLS.
fn upgrade(raw: &str, cfg: &WebConfig) -> String {
    let Some(rest) = raw
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .map(|_| &raw[7..])
    else {
        return raw.to_string();
    };
    if cfg.policy.allow_private_network
        && let Ok(u) = Url::parse(raw)
    {
        let local = match u.host() {
            Some(url::Host::Ipv4(ip)) => ip_blocked(ip.into()),
            Some(url::Host::Ipv6(ip)) => ip_blocked(ip.into()),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if local {
            return raw.to_string();
        }
    }
    format!("https://{rest}")
}

pub(crate) fn fetch<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let Some(raw) = str_field(input, "url") else {
            return ToolOutput::error("WebFetch: \"url\" is required.");
        };
        let cfg = lock(&sh.web_cfg).clone();
        let url_s = upgrade(raw, &cfg);
        let url = match Url::parse(&url_s) {
            Ok(u) => u,
            Err(e) => return err("invalid_url", format!("WebFetch: invalid URL: {e}")),
        };
        let f = match Fetcher::new(&env_of(ctx), cfg.policy) {
            Ok(f) => f,
            Err(e) => return err("fetch_error", format!("WebFetch: {e}")),
        };
        let r = tokio::select! {
            r = f.get(url, MAX_BYTES, TIMEOUT) => r,
            () = ctx.cancel.cancelled() => return ToolOutput::error("WebFetch: aborted."),
        };
        let page = match r {
            Ok(p) => p,
            Err(m) if m == "timed out" => {
                return err(
                    "timeout",
                    format!("WebFetch: timed out or aborted fetching {url_s}."),
                );
            }
            Err(m) => return err(kind_of(&m), format!("WebFetch: {m} (fetching {url_s})")),
        };
        if !page.status.is_success() {
            return err(
                "http_status",
                format!(
                    "WebFetch: {} {} for {}",
                    page.status.as_u16(),
                    page.status.canonical_reason().unwrap_or(""),
                    page.url
                ),
            );
        }
        if !textual(&page.content_type) {
            return err(
                "unsupported_content",
                format!(
                    "WebFetch: unsupported content type \"{}\" for {} (only text content is returned).",
                    page.content_type, page.url
                ),
            );
        }
        let body = decode_body(&page.body, &page.content_type);
        let is_html = page.content_type.to_ascii_lowercase().contains("html")
            || body
                .chars()
                .take(500)
                .collect::<String>()
                .to_ascii_lowercase()
                .contains("<html");
        let mut text = truncate_chars(if is_html { html_to_text(&body) } else { body });
        if page.truncated {
            text += "\n\n[truncated: the page exceeds the 5MB limit]";
        }
        if page.url.as_str() != url_s {
            text = format!("[redirected to {}]\n{text}", page.url);
        }
        ToolOutput::text(format!(
            "URL: {url_s}\n\n{}\n\n{}",
            input["prompt"].as_str().unwrap_or(""),
            untrusted(text)
        ))
        .with_meta(json!({"status": page.status.as_u16(), "bytes": page.body.len(), "truncated": page.truncated, "final_url": page.url.as_str()}))
    })
}

// ---- WebSearch ----

static ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
        .unwrap()
});
static SNIPPET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap()
});
static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn clean(s: &str) -> String {
    let t = decode_entities(&TAG.replace_all(s, ""));
    WS.replace_all(&t, " ").trim().to_string()
}

/// DuckDuckGo wraps each link as `//duckduckgo.com/l/?uddg=<encoded target>&rut=...`.
fn real_url(href: &str) -> String {
    let h = clean(href);
    let abs = if let Some(r) = h.strip_prefix("//") {
        format!("https://{r}")
    } else if h.starts_with('/') {
        format!("https://duckduckgo.com{h}")
    } else {
        h
    };
    Url::parse(&abs)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "uddg")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or(abs)
}

struct Hit {
    title: String,
    url: String,
    snippet: String,
}

fn parse_results(html: &str) -> Vec<Hit> {
    let ms: Vec<_> = ANCHOR.captures_iter(html).collect();
    let mut out = vec![];
    for (i, m) in ms.iter().enumerate() {
        let end = m.get(0).map_or(0, |g| g.end());
        let next = ms
            .get(i + 1)
            .and_then(|n| n.get(0))
            .map_or(html.len(), |g| g.start());
        let url = real_url(&m[1]);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        out.push(Hit {
            title: clean(&m[2]),
            url,
            snippet: SNIPPET
                .captures(&html[end..next])
                .map_or(String::new(), |s| clean(&s[1])),
        });
    }
    out
}

fn host_matches(host: &str, domain: &str) -> bool {
    let d = domain
        .trim()
        .trim_start_matches("www.")
        .to_ascii_lowercase();
    host == d || host.ends_with(&format!(".{d}"))
}

fn filter(hits: Vec<Hit>, allowed: &[String], blocked: &[String]) -> Vec<Hit> {
    hits.into_iter()
        .filter(|h| {
            let host = Url::parse(&h.url)
                .ok()
                .and_then(|u| {
                    u.host_str()
                        .map(|s| s.trim_start_matches("www.").to_ascii_lowercase())
                })
                .unwrap_or_default();
            (allowed.is_empty() || allowed.iter().any(|d| host_matches(&host, d)))
                && !blocked.iter().any(|d| host_matches(&host, d))
        })
        .collect()
}

fn strings(input: &Value, key: &str) -> Vec<String> {
    input[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn search_with(
    f: &Fetcher,
    endpoint: &str,
    input: &Value,
    timeout: Duration,
) -> ToolOutput {
    let query = input["query"].as_str().unwrap_or("").trim();
    if query.chars().count() < 2 {
        return ToolOutput::error("WebSearch: \"query\" is required (at least 2 characters).");
    }
    let mut url = Url::parse(endpoint).expect("endpoint");
    url.query_pairs_mut().append_pair("q", query);
    let page = match f.get(url, SEARCH_MAX_BYTES, timeout).await {
        Ok(p) => p,
        Err(m) if m == "timed out" => {
            return err("timeout", "WebSearch: timed out or aborted.".into());
        }
        Err(m) => return err(kind_of(&m), format!("WebSearch: {m}")),
    };
    if !page.status.is_success() {
        return err(
            "http_status",
            format!(
                "WebSearch: the search engine answered {} {}",
                page.status.as_u16(),
                page.status.canonical_reason().unwrap_or("")
            ),
        );
    }
    let found = parse_results(&decode_body(&page.body, &page.content_type));
    let n_found = found.len();
    let hits: Vec<Hit> = filter(
        found,
        &strings(input, "allowed_domains"),
        &strings(input, "blocked_domains"),
    )
    .into_iter()
    .take(MAX_RESULTS)
    .collect();
    if hits.is_empty() {
        let why = if n_found > 0 {
            "every result was removed by allowed_domains / blocked_domains"
        } else {
            "no results could be parsed (the search page may have changed, or the engine asked for a captcha)"
        };
        let msg = format!("WebSearch: nothing found for \"{query}\": {why}.");
        return if n_found > 0 {
            ToolOutput::text(msg)
        } else {
            err("no_results", msg)
        };
    }
    let lines: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let s = if h.snippet.is_empty() {
                String::new()
            } else {
                format!("\n   {}", h.snippet)
            };
            format!("{}. {}\n   {}{s}", i + 1, h.title, h.url)
        })
        .collect();
    ToolOutput::text(format!(
        "Search results for \"{query}\":\n\n{}\n\nUse WebFetch on a URL to read the page.",
        untrusted(lines.join("\n\n"))
    ))
    .with_meta(json!({"results": hits.len()}))
}

pub(crate) fn search<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        // The endpoint is fixed and trusted: the fetch allow list must not lock the search out.
        let mut policy = lock(&sh.web_cfg).policy.clone();
        policy.allow_domains.clear();
        let f = match Fetcher::new(&env_of(ctx), policy) {
            Ok(f) => f,
            Err(e) => return err("fetch_error", format!("WebSearch: {e}")),
        };
        tokio::select! {
            r = search_with(&f, SEARCH_ENDPOINT, input, SEARCH_TIMEOUT) => r,
            () = ctx.cancel.cancelled() => ToolOutput::error("WebSearch: aborted."),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    enum Resp {
        Raw(Vec<u8>),
        Hang,
        /// Endless body; the counter receives the bytes written.
        Flood(Arc<AtomicUsize>),
    }

    fn http(status: &str, headers: &[&str], body: &[u8]) -> Resp {
        let mut v = format!("HTTP/1.1 {status}\r\nConnection: close\r\n").into_bytes();
        for h in headers {
            v.extend(format!("{h}\r\n").bytes());
        }
        v.extend(format!("Content-Length: {}\r\n\r\n", body.len()).bytes());
        v.extend(body);
        Resp::Raw(v)
    }

    async fn serve(h: impl Fn(&str) -> Resp + Send + Sync + 'static) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let h = Arc::new(h);
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else {
                    return;
                };
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    match h(&path) {
                        Resp::Raw(v) => {
                            let _ = s.write_all(&v).await;
                        }
                        Resp::Hang => tokio::time::sleep(Duration::from_secs(30)).await,
                        Resp::Flood(count) => {
                            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\n\r\n").await;
                            let chunk = vec![b'a'; 64 * 1024];
                            for _ in 0..1024 {
                                if s.write_all(&chunk).await.is_err() {
                                    break;
                                }
                                count.fetch_add(chunk.len(), SeqCst);
                            }
                        }
                    }
                });
            }
        });
        port
    }

    fn lookup(rebind: Arc<AtomicUsize>) -> Lookup {
        Arc::new(move |host| {
            let rebind = rebind.clone();
            Box::pin(async move {
                let pub_ip: IpAddr = "127.0.0.1".parse().unwrap(); // "public" for these tests
                let priv_ip: IpAddr = "10.0.0.9".parse().unwrap();
                Ok(match host.as_str() {
                    "fix.test" => vec![pub_ip],
                    "evil.test" => vec![priv_ip],
                    "mixed.test" => vec![pub_ip, priv_ip],
                    "rebind.test" if rebind.fetch_add(1, SeqCst) == 0 => vec![pub_ip],
                    "rebind.test" => vec![priv_ip],
                    _ => vec![],
                })
            })
        })
    }

    /// 127.0.0.1 stands in for a public address; everything else keeps the real rules.
    fn test_blocked(ip: IpAddr) -> bool {
        ip != IpAddr::from([127, 0, 0, 1]) && ip_blocked(ip)
    }

    fn fetcher() -> Fetcher {
        Fetcher::with(
            &Env::default(),
            UrlPolicy::default(),
            lookup(Arc::default()),
            test_blocked,
        )
        .unwrap()
    }

    fn u(port: u16, host: &str, path: &str) -> Url {
        Url::parse(&format!("http://{host}:{port}{path}")).unwrap()
    }

    const T: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn fetches_gzip_and_decodes_charset() {
        let mut gz = GzEncoder::new(vec![], Compression::default());
        gz.write_all(b"zipped body").unwrap();
        let gz = gz.finish().unwrap();
        let port = serve(move |p| match p {
            "/gz" => http(
                "200 OK",
                &["Content-Type: text/plain", "Content-Encoding: gzip"],
                &gz,
            ),
            "/latin" => http(
                "200 OK",
                &["Content-Type: text/plain; charset=iso-8859-1"],
                b"caf\xe9",
            ),
            "/meta" => http(
                "200 OK",
                &["Content-Type: text/html"],
                b"<meta charset=\"windows-1252\">caf\xe9",
            ),
            _ => http("404 Not Found", &[], b""),
        })
        .await;
        let f = fetcher();
        let r = f.get(u(port, "fix.test", "/gz"), 1 << 20, T).await.unwrap();
        assert_eq!(r.body, b"zipped body");
        let r = f
            .get(u(port, "fix.test", "/latin"), 1 << 20, T)
            .await
            .unwrap();
        assert_eq!(decode_body(&r.body, &r.content_type), "café");
        let r = f
            .get(u(port, "fix.test", "/meta"), 1 << 20, T)
            .await
            .unwrap();
        assert!(decode_body(&r.body, &r.content_type).ends_with("café"));
        let r = f
            .get(u(port, "fix.test", "/nope"), 1 << 20, T)
            .await
            .unwrap();
        assert_eq!(r.status.as_u16(), 404);
    }

    #[tokio::test]
    async fn follows_redirects_but_rechecks_every_hop() {
        let port = serve(|p| match p {
            "/ok" => http("302 Found", &["Location: /end"], b""),
            "/end" => http("200 OK", &["Content-Type: text/plain"], b"end"),
            "/meta" => http(
                "302 Found",
                &["Location: http://169.254.169.254/latest/meta-data"],
                b"",
            ),
            "/lit6" => http("302 Found", &["Location: http://[::ffff:127.0.0.2]/"], b""),
            "/evil" => http("302 Found", &["Location: http://evil.test/"], b""),
            "/file" => http("302 Found", &["Location: file:///etc/passwd"], b""),
            "/loop" => http("302 Found", &["Location: /loop"], b""),
            _ => http("404 Not Found", &[], b""),
        })
        .await;
        let f = fetcher();
        let r = f.get(u(port, "fix.test", "/ok"), 1 << 20, T).await.unwrap();
        assert_eq!((r.body.as_slice(), r.url.path()), (&b"end"[..], "/end"));
        for (p, want) in [
            ("/meta", "private"),
            ("/lit6", "private"),
            ("/evil", "private"),
            ("/file", "not allowed"),
            ("/loop", "too many redirects"),
        ] {
            let e = f
                .get(u(port, "fix.test", p), 1 << 20, T)
                .await
                .err()
                .unwrap();
            assert!(e.contains(want), "{p}: {e}");
        }
    }

    #[tokio::test]
    async fn every_resolved_address_is_validated_and_rebinding_is_caught() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let port = serve(move |_| {
            h.fetch_add(1, SeqCst);
            http("200 OK", &["Content-Type: text/plain"], b"hi")
        })
        .await;
        let f = fetcher();
        for host in ["evil.test", "mixed.test"] {
            let e = f.get(u(port, host, "/"), 1 << 20, T).await.err().unwrap();
            assert!(e.contains("private/loopback/metadata"), "{host}: {e}");
        }
        assert_eq!(hits.load(SeqCst), 0, "no request reached the server");
        // First answer is fine, the "rebound" second answer is private: it is checked again.
        assert!(f.get(u(port, "rebind.test", "/"), 1 << 20, T).await.is_ok());
        let e = f
            .get(u(port, "rebind.test", "/"), 1 << 20, T)
            .await
            .err()
            .unwrap();
        assert!(e.contains("private/loopback/metadata"), "{e}");
        assert_eq!(hits.load(SeqCst), 1);
    }

    #[tokio::test]
    async fn oversize_body_aborts_and_slow_server_times_out() {
        let sent = Arc::new(AtomicUsize::new(0));
        let s = sent.clone();
        let port = serve(move |p| {
            if p == "/hang" {
                Resp::Hang
            } else {
                Resp::Flood(s.clone())
            }
        })
        .await;
        let f = fetcher();
        let r = f
            .get(u(port, "fix.test", "/big"), 1 << 20, T)
            .await
            .unwrap();
        assert!(r.truncated);
        assert_eq!(r.body.len(), 1 << 20);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            sent.load(SeqCst) < 32 << 20,
            "server was not drained: {}",
            sent.load(SeqCst)
        );
        let e = f
            .get(
                u(port, "fix.test", "/hang"),
                1 << 20,
                Duration::from_millis(300),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(e, "timed out");
    }

    #[test]
    fn url_checks_and_ip_table() {
        let f = Fetcher::new(&Env::default(), UrlPolicy::default()).unwrap();
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "http://user:pw@example.com/",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://localhost/",
            "http://169.254.169.254/",
            "http://0.0.0.0/",
            "http://224.0.0.1/",
            "http://[ff02::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:10.0.0.1]/",
            "http://[64:ff9b::7f00:1]/",
            "http://2130706433/",
            "http://100.64.0.1/",
        ] {
            assert!(f.check_url(&Url::parse(bad).unwrap()).is_err(), "{bad}");
        }
        assert!(
            f.check_url(&Url::parse("https://example.com/a").unwrap())
                .is_ok()
        );
        assert!(f.check_url(&Url::parse("http://8.8.8.8/").unwrap()).is_ok());
        assert!(!ip_blocked("2606:4700::1111".parse().unwrap()));
        for (ip, blocked) in [
            ("2001:0:4136:e378:8000:63bf:3fff:fdd2", true), // Teredo
            ("2001::1", true),
            ("2001:db8::1", true), // documentation range
            ("2001:4860:4860::8888", false),
            ("2001:1::1", false),
        ] {
            assert_eq!(ip_blocked(ip.parse().unwrap()), blocked, "{ip}");
        }
        let open = Fetcher::new(
            &Env::default(),
            UrlPolicy {
                allow_private_network: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            open.check_url(&Url::parse("http://127.0.0.1:3000/").unwrap())
                .is_ok()
        );
        let lists = Fetcher::new(
            &Env::default(),
            UrlPolicy {
                allow_domains: vec!["example.com".into()],
                deny_domains: vec!["bad.example.com".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            lists
                .check_url(&Url::parse("https://a.example.com/").unwrap())
                .is_ok()
        );
        assert!(
            lists
                .check_url(&Url::parse("https://bad.example.com/").unwrap())
                .is_err()
        );
        assert!(
            lists
                .check_url(&Url::parse("https://other.org/").unwrap())
                .is_err()
        );
    }

    #[test]
    fn upgrade_keeps_opted_in_local_http() {
        let def = WebConfig::default();
        assert_eq!(
            upgrade("HTTP://example.com/x", &def),
            "https://example.com/x"
        );
        assert_eq!(
            upgrade("http://127.0.0.1:3000/", &def),
            "https://127.0.0.1:3000/"
        );
        let open = WebConfig {
            policy: UrlPolicy {
                allow_private_network: true,
                ..Default::default()
            },
        };
        assert_eq!(
            upgrade("http://127.0.0.1:3000/", &open),
            "http://127.0.0.1:3000/"
        );
        assert_eq!(
            upgrade("http://example.com/", &open),
            "https://example.com/"
        );
    }

    #[test]
    fn html_becomes_text() {
        let t = html_to_text(
            "<html><style>p{}</style><script>x()</script><!-- c --><p>Hi &amp; &#x41;&#66; &nbsp;there &bogus;</p>\n\n\n\n<b>end</b></html>",
        );
        assert_eq!(t, "Hi & AB there &bogus;\n\nend");
    }

    const DDG: &str = r##"<div><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Fx&amp;rut=1">Docs <b>X</b></a>
<a class="result__snippet" href="#">The &quot;docs&quot;</a></div>
<div><a rel="x" class="result__a" href="https://other.org/p">Other</a></div>
<div><a class="result__a" href="javascript:alert(1)">Bad</a></div>"##;

    #[test]
    fn search_parsing_and_filtering() {
        let hits = parse_results(DDG);
        assert_eq!(hits.len(), 2);
        assert_eq!(
            (hits[0].title.as_str(), hits[0].url.as_str()),
            ("Docs X", "https://docs.rs/x")
        );
        assert_eq!(hits[0].snippet, "The \"docs\"");
        assert_eq!(hits[1].snippet, "");
        let only = filter(parse_results(DDG), &["docs.rs".into()], &[]);
        assert_eq!(only.len(), 1);
        assert_eq!(
            filter(parse_results(DDG), &[], &["www.other.org".into()]).len(),
            1
        );
    }

    #[tokio::test]
    async fn search_end_to_end_wraps_results_as_untrusted() {
        let port = serve(|p| {
            assert!(p.starts_with("/html/?q=rust+lang"), "{p}");
            http("200 OK", &["Content-Type: text/html"], DDG.as_bytes())
        })
        .await;
        let f = fetcher();
        let ep = format!("http://fix.test:{port}/html/");
        let o = search_with(&f, &ep, &json!({"query": "rust lang"}), T).await;
        let crate::Content::Text(t) = &o.content else {
            panic!()
        };
        assert!(!o.is_error, "{t}");
        assert!(
            t.contains("<untrusted_page_content") && t.contains("1. Docs X\n   https://docs.rs/x"),
            "{t}"
        );
        let o = search_with(&f, &ep, &json!({"query": "x"}), T).await;
        assert!(o.is_error);
        let o = search_with(
            &f,
            &ep,
            &json!({"query": "rust lang", "allowed_domains": ["nope.io"]}),
            T,
        )
        .await;
        assert!(!o.is_error);
    }
}
