//! The address a remote MCP connection really goes to is the address that was checked: every name is
//! resolved HERE, every answer is filtered through `synthesis_platform::net::ip_blocked` (private, link-local,
//! the cloud metadata address, CGNAT, multicast, loopback, ...), and only the validated addresses are handed to
//! the connector, so a name that answers differently the second time (DNS rebinding) cannot lead to a private
//! host. The one exception is an explicit loopback NAME (`localhost`, which a person may write by hand): its
//! loopback answers are kept and nothing else of it is.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// How names are looked up (the system resolver; a test passes its own).
pub(crate) type Lookup = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>> + Send + Sync,
>;

pub(crate) fn system_lookup() -> Lookup {
    Arc::new(|host| {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    })
}

pub(crate) fn is_loopback_name(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
}

/// The addresses of `host` that may be connected to: for `localhost` its loopback ones, for any other name the
/// ones that are not blocked. `Err` when none is left (an empty answer included).
pub(crate) fn validated(host: &str, ips: Vec<IpAddr>) -> Result<Vec<IpAddr>, String> {
    let keep: Vec<IpAddr> = if is_loopback_name(host) {
        ips.into_iter().filter(IpAddr::is_loopback).collect()
    } else {
        ips.into_iter()
            .filter(|ip| !synthesis_platform::net::ip_blocked(*ip))
            .collect()
    };
    if keep.is_empty() {
        Err(format!(
            "{host} has no public address (private, internal or unresolvable)"
        ))
    } else {
        Ok(keep)
    }
}

/// Resolves `host` and returns only validated addresses.
pub(crate) async fn resolve_validated(host: &str, lookup: &Lookup) -> Result<Vec<IpAddr>, String> {
    let ips = lookup(host.to_string())
        .await
        .map_err(|_| format!("{host} does not resolve"))?;
    validated(host, ips)
}

/// For `reqwest`: the connector only ever sees validated addresses.
pub(crate) struct GuardResolver(pub Lookup);

impl GuardResolver {
    pub fn system() -> Arc<Self> {
        Arc::new(Self(system_lookup()))
    }
}

impl Resolve for GuardResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let lookup = self.0.clone();
        let host = name.as_str().to_ascii_lowercase();
        Box::pin(async move {
            let ips = resolve_validated(&host, &lookup).await?;
            Ok(Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_validated_addresses_survive() {
        let got = validated(
            "mcp.example.com",
            vec![
                ip("10.0.0.1"),
                ip("8.8.8.8"),
                ip("::1"),
                ip("169.254.169.254"),
            ],
        )
        .unwrap();
        assert_eq!(got, [ip("8.8.8.8")]);
        for all_bad in [
            vec![ip("10.0.0.1")],
            vec![ip("127.0.0.1")],
            vec![ip("fd00::1")],
            vec![],
        ] {
            assert!(validated("mcp.example.com", all_bad).is_err());
        }
        // `localhost` keeps its loopback answers and nothing else
        assert_eq!(
            validated("LOCALHOST", vec![ip("127.0.0.1"), ip("8.8.8.8")]).unwrap(),
            [ip("127.0.0.1")]
        );
        assert!(validated("localhost", vec![ip("10.0.0.1")]).is_err());
    }

    /// A name that answers with a private address the second time (rebinding) is never connected to: the
    /// connector asks THIS resolver, which filters, instead of resolving again by itself.
    #[tokio::test]
    async fn a_client_never_connects_to_a_name_that_resolves_private() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for _ in l.incoming().flatten() {
                h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let fake: Lookup = Arc::new(|_| Box::pin(async { Ok(vec!["127.0.0.1".parse().unwrap()]) }));
        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(GuardResolver(fake)))
            .build()
            .unwrap();
        let r = client
            .get(format!("http://rebind.test:{port}/"))
            .send()
            .await;
        assert!(r.is_err(), "the private answer was filtered out");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing connected"
        );
    }
}
