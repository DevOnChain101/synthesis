//! Per-run egress proxy: the only way out of a sandboxed command (docs/specs/sandbox-hardening.md).
//!
//! HTTP CONNECT only. Per connection, in this order: read the head (8 KiB, 10 s), authenticate
//! (TCP listeners), parse and normalise the target, check the allowlist BEFORE any DNS lookup
//! (an unlisted name is never resolved), resolve through the injected [`Lookup`] and refuse when
//! ANY address is private/loopback/link-local/metadata, then dial the validated IP itself (never
//! a second lookup) and copy bytes with an idle timeout. A hostname never reaches a private
//! address; a private or loopback destination needs an allowlist entry naming that exact IP
//! literal and port.
//!
//! An allowlist limits destinations. It does not stop data leaving to an allowed host that
//! accepts uploads.

mod allow;
mod head;

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use synthesis_events::Event;
use synthesis_platform::net::ip_blocked;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

use allow::split_host_port;
pub use allow::{Allowlist, Target, normalise_host};
use head::{HeadError, read_head};

pub const MAX_CONNECTIONS: usize = 64;
const IDLE: Duration = Duration::from_secs(300);
/// A tunnel never lives longer than this, active or not.
const MAX_LIFE: Duration = Duration::from_secs(3600);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Event budgets per window (a looping client must not flood the log): denials, and
/// allowed/closed pairs. Over budget the events are dropped and one `egress.suppressed` follows.
const DENY_EVENTS: u32 = 20;
const OPEN_EVENTS: u32 = 200;
const EVENT_WINDOW: Duration = Duration::from_secs(10);
/// Consecutive accept failures (after a 50 ms pause each) before the listener gives up.
const ACCEPT_FAILURES: u32 = 100;

pub type Lookup = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>> + Send + Sync,
>;
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// Resolver of the host (getaddrinfo through tokio).
pub fn system_lookup() -> Lookup {
    Arc::new(|host| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    })
}

#[derive(Default)]
pub struct Stats {
    pub allowed: AtomicU64,
    pub denied: AtomicU64,
    pub up: AtomicU64,
    pub down: AtomicU64,
}

/// Fixed-window event budget; what it drops is counted for the summary event.
struct Limiter {
    limit: u32,
    /// window start, events seen in the window, events dropped since the last summary
    st: Mutex<(Instant, u32, u64)>,
}

impl Limiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            st: Mutex::new((Instant::now(), 0, 0)),
        }
    }

    /// (send this event?, dropped count to report before it)
    fn admit(&self) -> (bool, u64) {
        let mut g = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let mut summary = 0;
        if g.0.elapsed() > EVENT_WINDOW {
            summary = std::mem::take(&mut g.2);
            *g = (Instant::now(), 0, 0);
        }
        g.1 += 1;
        if g.1 <= self.limit {
            (true, summary)
        } else {
            g.2 += 1;
            (false, summary)
        }
    }

    fn take_dropped(&self) -> u64 {
        std::mem::take(&mut self.st.lock().unwrap_or_else(|e| e.into_inner()).2)
    }
}

struct Inner {
    allow: Allowlist,
    lookup: Lookup,
    sink: Sink,
    /// Expected `Proxy-Authorization` value; `None` on unix-socket listeners.
    auth: Option<String>,
    slots: Arc<Semaphore>,
    idle: Duration,
    max_life: Duration,
    stats: Arc<Stats>,
    deny_events: Limiter,
    open_events: Limiter,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let n = self.deny_events.take_dropped() + self.open_events.take_dropped();
        if n > 0 {
            (self.sink)(Event::EgressSuppressed { count: n });
        }
    }
}

/// A proxy that is not listening yet.
pub struct Proxy {
    allow: Allowlist,
    lookup: Lookup,
    sink: Sink,
    max_conns: usize,
    idle: Duration,
    max_life: Duration,
}

/// A listening proxy; dropping it stops accepting and aborts every open connection (each open
/// tunnel still reports its `egress.closed`).
pub struct Running {
    pub stats: Arc<Stats>,
    /// TCP only: the loopback port and the Basic credential (`synt:<token>`).
    pub tcp: Option<(u16, String)>,
    task: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the accept loop needs from a client stream.
trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// Best-effort refusal without waiting (the slot limit is checked before a task exists).
    fn refuse(&self);
}

const BUSY: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

impl Conn for TcpStream {
    fn refuse(&self) {
        let _ = self.try_write(BUSY);
    }
}

#[cfg(unix)]
impl Conn for tokio::net::UnixStream {
    fn refuse(&self) {
        let _ = self.try_write(BUSY);
    }
}

impl Proxy {
    pub fn new(allow: Allowlist, lookup: Lookup, sink: Sink) -> Self {
        Self {
            allow,
            lookup,
            sink,
            max_conns: MAX_CONNECTIONS,
            idle: IDLE,
            max_life: MAX_LIFE,
        }
    }

    fn inner(self, auth: Option<String>) -> (Arc<Inner>, Arc<Stats>) {
        let stats = Arc::new(Stats::default());
        let inner = Arc::new(Inner {
            allow: self.allow,
            lookup: self.lookup,
            sink: self.sink,
            auth,
            slots: Arc::new(Semaphore::new(self.max_conns)),
            idle: self.idle,
            max_life: self.max_life,
            stats: stats.clone(),
            deny_events: Limiter::new(DENY_EVENTS),
            open_events: Limiter::new(OPEN_EVENTS),
        });
        (inner, stats)
    }

    /// Listen on `127.0.0.1:0` with a random Basic credential (macOS Seatbelt).
    pub async fn serve_tcp(self) -> io::Result<Running> {
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
        let token = URL_SAFE_NO_PAD.encode(raw);
        let auth = format!("Basic {}", STANDARD.encode(format!("synt:{token}")));
        let l = Arc::new(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?);
        let port = l.local_addr()?.port();
        let (inner, stats) = self.inner(Some(auth));
        let task = tokio::spawn(accept_loop(inner, move || {
            let l = l.clone();
            async move { l.accept().await.map(|(s, _)| s) }
        }));
        Ok(Running {
            stats,
            tcp: Some((port, token)),
            task,
        })
    }

    /// Listen on a unix socket; the caller makes its directory 0700 (Linux bwrap).
    #[cfg(unix)]
    pub async fn serve_unix(self, path: &std::path::Path) -> io::Result<Running> {
        let l = Arc::new(tokio::net::UnixListener::bind(path)?);
        let (inner, stats) = self.inner(None);
        let task = tokio::spawn(accept_loop(inner, move || {
            let l = l.clone();
            async move { l.accept().await.map(|(s, _)| s) }
        }));
        Ok(Running {
            stats,
            tcp: None,
            task,
        })
    }
}

/// Accept, take a connection slot BEFORE spawning (so a flood cannot create unbounded tasks), and
/// survive transient accept errors (EMFILE, ...): pause and retry, give up only after
/// [`ACCEPT_FAILURES`] in a row, and say so.
async fn accept_loop<S, F, Fut>(inner: Arc<Inner>, mut accept: F)
where
    S: Conn,
    F: FnMut() -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut conns = JoinSet::new();
    let mut failures = 0u32;
    loop {
        match accept().await {
            Ok(s) => {
                failures = 0;
                while conns.try_join_next().is_some() {}
                match inner.slots.clone().try_acquire_owned() {
                    Ok(permit) => {
                        conns.spawn(handle(inner.clone(), s, permit));
                    }
                    Err(_) => {
                        s.refuse();
                        inner.deny("", 0, "limit");
                    }
                }
            }
            Err(_) => {
                failures += 1;
                if failures >= ACCEPT_FAILURES {
                    (inner.sink)(Event::Notice {
                        level: "warning".into(),
                        text: "egress proxy stopped accepting connections (the listener kept failing); commands have no network until the run restarts".into(),
                    });
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// ASCII only: control characters, bidi/format characters and anything non-ASCII become escapes,
/// so a name chosen by the sandboxed command cannot spoof or break a log line.
fn clean(s: &str) -> String {
    s.chars().flat_map(char::escape_default).take(200).collect()
}

fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

impl Inner {
    fn send(&self, lim: &Limiter, ev: Event) {
        let (ok, dropped) = lim.admit();
        if dropped > 0 {
            (self.sink)(Event::EgressSuppressed { count: dropped });
        }
        if ok {
            (self.sink)(ev);
        }
    }

    fn deny(&self, host: &str, port: u16, reason: &str) {
        self.stats.denied.fetch_add(1, Relaxed);
        self.send(
            &self.deny_events,
            Event::EgressDenied {
                host: clean(host),
                port,
                reason: reason.into(),
            },
        );
    }
}

/// An open tunnel. Reports `egress.closed` when dropped, also when the run is shut down and the
/// connection task is aborted.
struct Open {
    inner: Arc<Inner>,
    host: String,
    port: u16,
    up: AtomicU64,
    down: AtomicU64,
    started: Instant,
}

impl Drop for Open {
    fn drop(&mut self) {
        let (up, down) = (self.up.load(Relaxed), self.down.load(Relaxed));
        self.inner.stats.up.fetch_add(up, Relaxed);
        self.inner.stats.down.fetch_add(down, Relaxed);
        self.inner.send(
            &self.inner.open_events,
            Event::EgressClosed {
                host: clean(&self.host),
                port: self.port,
                up,
                down,
                ms: self.started.elapsed().as_millis() as u64,
            },
        );
    }
}

async fn reply<S: AsyncWrite + Unpin>(s: &mut S, status: &str, extra: &str) {
    let msg = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
    let _ = tokio::time::timeout(Duration::from_secs(2), s.write_all(msg.as_bytes())).await;
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin>(
    inner: Arc<Inner>,
    mut s: S,
    _slot: OwnedSemaphorePermit,
) {
    let head = match read_head(&mut s).await {
        Ok(h) => h,
        Err(HeadError::Bad(_)) => {
            reply(&mut s, "400 Bad Request", "").await;
            return inner.deny("", 0, "bad_request");
        }
    };
    if let Some(want) = &inner.auth
        && !head.auth.as_deref().is_some_and(|got| ct_eq(got, want))
    {
        reply(
            &mut s,
            "407 Proxy Authentication Required",
            "Proxy-Authenticate: Basic realm=\"synt\"\r\n",
        )
        .await;
        return inner.deny("", 0, "auth");
    }
    let Ok((host, port)) = split_host_port(&head.authority, None) else {
        reply(&mut s, "400 Bad Request", "").await;
        return inner.deny(&head.authority, 0, "bad_request");
    };
    let Some(target) = normalise_host(host) else {
        reply(&mut s, "400 Bad Request", "").await;
        return inner.deny(host, port, "bad_request");
    };
    let shown = match &target {
        Target::Name(n) => n.clone(),
        Target::Ip(ip) => ip.to_string(),
    };
    // The allowlist comes first: an unlisted name is never resolved.
    if !inner.allow.allows(&target, port) {
        reply(&mut s, "403 Forbidden", "").await;
        return inner.deny(&shown, port, "unlisted");
    }
    let ips = match &target {
        Target::Ip(ip) => vec![*ip], // listed as this exact literal: private/loopback is the point
        Target::Name(n) => match (inner.lookup)(n.clone()).await {
            Ok(v) if !v.is_empty() => v,
            _ => {
                reply(&mut s, "502 Bad Gateway", "").await;
                return inner.deny(&shown, port, "resolve");
            }
        },
    };
    if matches!(target, Target::Name(_)) && ips.iter().any(|ip| ip_blocked(*ip)) {
        reply(&mut s, "403 Forbidden", "").await;
        return inner.deny(&shown, port, "private");
    }
    let mut upstream = None;
    for ip in &ips {
        if let Ok(Ok(u)) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(SocketAddr::new(*ip, port)),
        )
        .await
        {
            upstream = Some((u, *ip));
            break;
        }
    }
    let Some((mut up_s, ip)) = upstream else {
        reply(&mut s, "502 Bad Gateway", "").await;
        return inner.deny(&shown, port, "resolve");
    };
    if s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || (!head.rest.is_empty() && up_s.write_all(&head.rest).await.is_err())
    {
        return;
    }
    inner.stats.allowed.fetch_add(1, Relaxed);
    inner.send(
        &inner.open_events,
        Event::EgressAllowed {
            host: clean(&shown),
            port,
            ip: ip.to_string(),
        },
    );
    let open = Open {
        inner: inner.clone(),
        host: shown,
        port,
        up: AtomicU64::new(head.rest.len() as u64),
        down: AtomicU64::new(0),
        started: Instant::now(),
    };
    let last = Mutex::new(Instant::now());
    let (mut cr, mut cw) = tokio::io::split(s);
    let (mut ur, mut uw) = up_s.split();
    let watchdog = async {
        loop {
            let idle_for = last.lock().unwrap_or_else(|e| e.into_inner()).elapsed();
            if idle_for >= inner.idle {
                return;
            }
            tokio::time::sleep(inner.idle - idle_for).await;
        }
    };
    tokio::select! {
        _ = async { tokio::join!(pump(&mut cr, &mut uw, &open.up, &last), pump(&mut ur, &mut cw, &open.down, &last)) } => {}
        _ = watchdog => {}
        _ = tokio::time::sleep(inner.max_life) => {}
    }
}

/// Copy `r` to `w`, then half-close `w`; bumps the byte counter and the shared activity clock.
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
    bytes: &AtomicU64,
    last: &Mutex<Instant>,
) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        *last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        if w.write_all(&buf[..n]).await.is_err() {
            break;
        }
        bytes.fetch_add(n as u64, Relaxed);
    }
    let _ = w.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::net::TcpListener;

    type Events = Arc<Mutex<Vec<Event>>>;

    fn sink() -> (Sink, Events) {
        let ev: Events = Arc::default();
        let e2 = ev.clone();
        (Arc::new(move |e| e2.lock().unwrap().push(e)), ev)
    }

    fn fixed(ips: &[&str]) -> (Lookup, Arc<AtomicUsize>) {
        let ips: Vec<IpAddr> = ips.iter().map(|s| s.parse().unwrap()).collect();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        (
            Arc::new(move |_| {
                c.fetch_add(1, Relaxed);
                let ips = ips.clone();
                Box::pin(async move { Ok(ips) })
            }),
            calls,
        )
    }

    async fn echo_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut b = [0u8; 64];
                    while let Ok(n) = s.read(&mut b).await {
                        if n == 0 || s.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    /// Send `CONNECT authority` with the credential; return the status line.
    async fn connect(port: u16, token: Option<&str>, authority: &str) -> (TcpStream, String) {
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let auth = token
            .map(|t| {
                format!(
                    "Proxy-Authorization: Basic {}\r\n",
                    STANDARD.encode(format!("synt:{t}"))
                )
            })
            .unwrap_or_default();
        c.write_all(
            format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth}\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut got = vec![];
        let mut b = [0u8; 1];
        while !got.ends_with(b"\r\n\r\n") {
            if c.read(&mut b).await.unwrap() == 0 {
                break;
            }
            got.push(b[0]);
        }
        let line = String::from_utf8_lossy(&got)
            .lines()
            .next()
            .unwrap_or("")
            .to_owned();
        (c, line)
    }

    async fn start(allow: &[&str], lookup: Lookup) -> (Running, Events, u16, String) {
        let (sk, ev) = sink();
        let r = Proxy::new(Allowlist::parse(allow).unwrap(), lookup, sk)
            .serve_tcp()
            .await
            .unwrap();
        let (port, token) = r.tcp.clone().unwrap();
        (r, ev, port, token)
    }

    #[tokio::test]
    async fn round_trip_to_an_allowed_loopback_literal() {
        let echo = echo_server().await;
        let (r, ev, port, token) = start(&[&format!("127.0.0.1:{echo}")], fixed(&[]).0).await;
        let (mut c, line) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        assert!(line.contains("200"), "{line}");
        c.write_all(b"ping").await.unwrap();
        let mut b = [0u8; 4];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping");
        // opened: reported at the 200, before any close
        assert!(
            matches!(&ev.lock().unwrap()[0], Event::EgressAllowed { host, port, ip }
            if host == "127.0.0.1" && *port == echo && ip == "127.0.0.1")
        );
        drop(c);
        tokio::time::timeout(Duration::from_secs(5), async {
            while ev.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        match &ev.lock().unwrap()[1] {
            Event::EgressClosed {
                host,
                port,
                up,
                down,
                ..
            } => {
                assert_eq!(
                    (host.as_str(), *port, *up, *down),
                    ("127.0.0.1", echo, 4, 4)
                );
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(r.stats.allowed.load(Relaxed), 1);
    }

    #[tokio::test]
    async fn other_loopback_port_and_unlisted_names_are_denied_without_dns() {
        let echo = echo_server().await;
        let (lookup, calls) = fixed(&["93.184.216.34"]);
        let (_r, ev, port, token) = start(&[&format!("127.0.0.1:{echo}"), "ok.test"], lookup).await;
        let (_c, line) = connect(
            port,
            Some(&token),
            &format!("127.0.0.1:{}", echo.wrapping_add(1)),
        )
        .await;
        assert!(line.contains("403"), "{line}");
        let (_c, line) = connect(port, Some(&token), "evil.test:443").await;
        assert!(line.contains("403"), "{line}");
        assert_eq!(
            calls.load(Relaxed),
            0,
            "an unlisted name must never be resolved"
        );
        let reasons: Vec<_> = ev
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                Event::EgressDenied { reason, .. } => reason.clone(),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(reasons, ["unlisted", "unlisted"]);
    }

    #[tokio::test]
    async fn a_hostname_never_reaches_a_private_address() {
        for answer in [
            vec!["127.0.0.1"],
            vec!["169.254.169.254"],
            vec!["::ffff:169.254.169.254"],
            vec!["::ffff:127.0.0.1"],
            vec!["10.0.0.5"],
            vec!["93.184.216.34", "10.0.0.5"], // ANY blocked address denies
        ] {
            let (lookup, calls) = fixed(&answer);
            let (_r, ev, port, token) = start(&["rebind.test"], lookup).await;
            let (_c, line) = connect(port, Some(&token), "rebind.test:443").await;
            assert!(line.contains("403"), "{answer:?}: {line}");
            assert_eq!(calls.load(Relaxed), 1);
            assert!(
                matches!(&ev.lock().unwrap()[0], Event::EgressDenied { reason, .. } if reason == "private")
            );
        }
    }

    #[tokio::test]
    async fn idle_connections_are_closed() {
        let echo = echo_server().await;
        let (sk, ev) = sink();
        let mut p = Proxy::new(
            Allowlist::parse(&[format!("127.0.0.1:{echo}")]).unwrap(),
            fixed(&[]).0,
            sk,
        );
        p.idle = Duration::from_millis(150);
        let r = p.serve_tcp().await.unwrap();
        let (port, token) = r.tcp.clone().unwrap();
        let (mut c, line) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        assert!(line.contains("200"));
        let mut b = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut b))
            .await
            .unwrap();
        assert!(matches!(n, Ok(0) | Err(_)));
        assert!(matches!(ev.lock().unwrap()[0], Event::EgressAllowed { .. }));
    }

    #[tokio::test]
    async fn credential_and_request_shape_are_enforced() {
        let (_r, ev, port, token) = start(&["a.test"], fixed(&[]).0).await;
        assert!(connect(port, None, "a.test:443").await.1.contains("407"));
        assert!(
            connect(port, Some("wrong"), "a.test:443")
                .await
                .1
                .contains("407")
        );
        assert!(
            connect(port, Some(&token), "a.test")
                .await
                .1
                .contains("400")
        );
        assert!(
            connect(port, Some(&token), "b\u{fc}cher.test:443")
                .await
                .1
                .contains("400")
        );
        let evs = ev.lock().unwrap();
        let reasons: Vec<_> = evs
            .iter()
            .map(|e| match e {
                Event::EgressDenied { reason, .. } => reason.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(reasons, ["auth", "auth", "bad_request", "bad_request"]);
        assert!(!format!("{evs:?}").contains(&token));
    }

    #[tokio::test]
    async fn too_many_connections_are_refused() {
        let echo = echo_server().await;
        let (sk, ev) = sink();
        let mut p = Proxy::new(
            Allowlist::parse(&[format!("127.0.0.1:{echo}")]).unwrap(),
            fixed(&[]).0,
            sk,
        );
        p.max_conns = 1;
        let r = p.serve_tcp().await.unwrap();
        let (port, token) = r.tcp.clone().unwrap();
        let (_held, line) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        assert!(line.contains("200"));
        let (_c, line) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        assert!(line.contains("503"), "{line}");
        assert!(
            ev.lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::EgressDenied { reason, .. } if reason == "limit"))
        );
    }

    #[tokio::test]
    async fn stopping_the_proxy_still_reports_the_close() {
        let echo = echo_server().await;
        let (r, ev, port, token) = start(&[&format!("127.0.0.1:{echo}")], fixed(&[]).0).await;
        let (mut c, _) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        c.write_all(b"abc").await.unwrap();
        let mut b = [0u8; 3];
        c.read_exact(&mut b).await.unwrap();
        drop(r); // the run ends with the tunnel open
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ev
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::EgressClosed { up: 3, down: 3, .. }))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("close event flushed on abort");
    }

    #[tokio::test]
    async fn tunnels_end_at_the_maximum_lifetime_even_when_active() {
        let echo = echo_server().await;
        let (sk, ev) = sink();
        let mut p = Proxy::new(
            Allowlist::parse(&[format!("127.0.0.1:{echo}")]).unwrap(),
            fixed(&[]).0,
            sk,
        );
        p.max_life = Duration::from_millis(200);
        let r = p.serve_tcp().await.unwrap();
        let (port, token) = r.tcp.clone().unwrap();
        let (mut c, _) = connect(port, Some(&token), &format!("127.0.0.1:{echo}")).await;
        let mut closed = false;
        for _ in 0..100 {
            if c.write_all(b"x").await.is_err() {
                closed = true;
                break;
            }
            let mut b = [0u8; 1];
            if !matches!(c.read(&mut b).await, Ok(1)) {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(closed, "an active tunnel must still end");
        assert!(
            ev.lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::EgressClosed { .. }))
        );
    }

    #[test]
    fn the_limiter_drops_over_budget_and_reports_the_count_once() {
        let l = Limiter::new(2);
        assert_eq!(l.admit(), (true, 0));
        assert_eq!(l.admit(), (true, 0));
        assert_eq!(l.admit(), (false, 0));
        assert_eq!(l.admit(), (false, 0));
        assert_eq!(l.take_dropped(), 2);
        assert_eq!(l.take_dropped(), 0);
    }

    #[tokio::test]
    async fn a_flood_of_denials_is_cut_and_summarised_on_shutdown() {
        let (r, ev, port, token) = start(&["a.test"], fixed(&[]).0).await;
        for _ in 0..(DENY_EVENTS + 5) {
            let _ = connect(port, Some(&token), "x.test:443").await;
        }
        drop(r);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ev
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::EgressSuppressed { count: 5 }))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("suppressed summary");
        let denied = ev
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::EgressDenied { .. }))
            .count();
        assert_eq!(denied as u32, DENY_EVENTS);
    }

    #[test]
    fn hosts_in_events_are_ascii_escaped() {
        let c = clean("a\u{202e}b\u{200b}\n\u{fc}.test");
        assert!(
            c.is_ascii() && !c.contains('\n') && !c.contains('\u{202e}'),
            "{c}"
        );
        assert!(c.contains("202e") && c.contains("200b"));
        assert_eq!(clean(&"a".repeat(500)).len(), 200);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_needs_no_credential() {
        let echo = echo_server().await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("e.sock");
        let (sk, _ev) = sink();
        let _r = Proxy::new(
            Allowlist::parse(&[format!("127.0.0.1:{echo}")]).unwrap(),
            fixed(&[]).0,
            sk,
        )
        .serve_unix(&sock)
        .await
        .unwrap();
        let mut c = tokio::net::UnixStream::connect(&sock).await.unwrap();
        c.write_all(format!("CONNECT 127.0.0.1:{echo} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut b = [0u8; 39];
        c.read_exact(&mut b).await.unwrap();
        assert!(String::from_utf8_lossy(&b).contains("200"));
    }
}
