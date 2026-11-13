//! `--network hosts`: the per-run egress proxy that is the only way out of the sandbox
//! (docs/specs/sandbox-hardening.md). The sandbox crate only plumbs the endpoint; the host list
//! stays here, in the proxy.

use std::sync::{Arc, Mutex};

use synthesis_config::Env;
use synthesis_egress::{Allowlist, Proxy, Running, system_lookup};
use synthesis_harness::{EventSink, Scope};
use synthesis_sandbox::EgressEndpoint;
use synthesis_settings::SynthesisSettings;

/// A running proxy: stops (and closes every tunnel) when dropped.
pub struct EgressRun {
    _running: Running,
    rt: Option<tokio::runtime::Runtime>,
    /// Holds the 0700 directory of the unix socket (bubblewrap).
    _dir: Option<tempfile::TempDir>,
    pub hosts: Vec<String>,
    /// Entries that name a private, loopback or link-local address (`ip:port`), for the start notice.
    pub private_ips: Vec<String>,
}

impl Drop for EgressRun {
    fn drop(&mut self) {
        // the proxy tasks end with `_running`; never block a caller that sits inside a runtime
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// The hosts of this run. Managed settings fix the list (the command line cannot add to it);
/// otherwise the user's list plus `--allow-host`. A project's list is never read. Refused when
/// empty, malformed, or when a host proxy is configured (chaining through it is not supported yet).
pub fn effective_hosts(
    cli: &[String],
    s: &SynthesisSettings,
    env: &Env,
) -> Result<Vec<String>, String> {
    let mut hosts = s.network_allowed_hosts.clone().unwrap_or_default();
    if s.network_allowed_hosts_managed {
        if !cli.is_empty() {
            return Err("--allow-host is refused: managed settings fix the allowed hosts (synt.sandbox.network.allowedHosts)".into());
        }
    } else {
        hosts.extend(cli.iter().cloned());
    }
    hosts.dedup();
    if hosts.is_empty() {
        return Err("--network hosts needs at least one host: pass --allow-host HOST (repeatable) or set synt.sandbox.network.allowedHosts in your user settings".into());
    }
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        if env
            .get_nonempty(k)
            .or(env.get_nonempty(&k.to_lowercase()))
            .is_some()
        {
            return Err(format!(
                "--network hosts cannot chain through the proxy set in {k} yet; unset it for this run or use --network deny"
            ));
        }
    }
    Allowlist::parse(&hosts)
        .map_err(|e| format!("--allow-host / synt.sandbox.network.allowedHosts: {e}"))?;
    Ok(hosts)
}

/// Start the proxy for the backend that will run the commands. `slot` is filled with the run's
/// event sink later; `egress.*` events go there.
pub fn start(
    hosts: Vec<String>,
    backend: &str,
    slot: Arc<Mutex<Option<EventSink>>>,
) -> Result<(EgressRun, EgressEndpoint), String> {
    let unix = match backend {
        "bubblewrap" => true,
        "seatbelt" => false,
        other => {
            return Err(format!(
                "--network hosts needs bubblewrap (Linux, WSL2) or Seatbelt (macOS); the sandbox here is {other}, which cannot confine commands to the proxy"
            ));
        }
    };
    let allow = Allowlist::parse(&hosts)?;
    let private_ips = allow
        .private_ips()
        .into_iter()
        .map(|(ip, port)| match ip {
            std::net::IpAddr::V4(_) => format!("{ip}:{port}"),
            std::net::IpAddr::V6(_) => format!("[{ip}]:{port}"),
        })
        .collect();
    let sink: synthesis_egress::Sink = Arc::new(move |e| {
        if let Some(s) = slot.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            s.emit(&Scope::default(), e);
        }
    });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("egress proxy: {e}"))?;
    let proxy = Proxy::new(allow, system_lookup(), sink);
    let (running, dir, ep) = if unix {
        #[cfg(unix)]
        {
            let dir = tempfile::Builder::new()
                .prefix("synt-egress-")
                .tempdir()
                .map_err(|e| format!("egress proxy: {e}"))?; // 0700
            let sock = dir.path().join("e.sock");
            let r = rt
                .block_on(proxy.serve_unix(&sock))
                .map_err(|e| format!("egress proxy: {e}"))?;
            (r, Some(dir), EgressEndpoint::Unix(sock))
        }
        #[cfg(not(unix))]
        {
            return Err("egress proxy: unix sockets are not available here".into());
        }
    } else {
        let r = rt
            .block_on(proxy.serve_tcp())
            .map_err(|e| format!("egress proxy: {e}"))?;
        let (port, token) = r.tcp.clone().ok_or("egress proxy: no port")?;
        (r, None, EgressEndpoint::Tcp { port, token })
    };
    Ok((
        EgressRun {
            _running: running,
            rt: Some(rt),
            _dir: dir,
            private_ips,
            hosts,
        },
        ep,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(user: Option<&[&str]>, managed: bool) -> SynthesisSettings {
        SynthesisSettings {
            network_allowed_hosts: user.map(|u| u.iter().map(|h| h.to_string()).collect()),
            network_allowed_hosts_managed: managed,
            ..Default::default()
        }
    }
    fn cli(h: &[&str]) -> Vec<String> {
        h.iter().map(|h| h.to_string()).collect()
    }

    #[test]
    fn hosts_combine_user_and_cli_but_managed_is_final() {
        let e = Env::default();
        assert_eq!(
            effective_hosts(&cli(&["b.test"]), &s(Some(&["a.test"]), false), &e).unwrap(),
            ["a.test", "b.test"]
        );
        assert_eq!(
            effective_hosts(&[], &s(Some(&["corp.test"]), true), &e).unwrap(),
            ["corp.test"]
        );
        assert!(effective_hosts(&cli(&["b.test"]), &s(Some(&["corp.test"]), true), &e).is_err());
        // nothing listed, a malformed entry, or a host proxy upstream: refused
        assert!(effective_hosts(&[], &s(None, false), &e).is_err());
        assert!(effective_hosts(&cli(&["*.com"]), &s(None, false), &e).is_err());
        let proxied = Env::from_pairs([("https_proxy", "http://p:8080")]);
        let err = effective_hosts(&cli(&["a.test"]), &s(None, false), &proxied).unwrap_err();
        assert!(err.contains("HTTPS_PROXY") && !err.contains("p:8080"));
    }

    #[cfg(unix)]
    #[test]
    fn private_literal_entries_are_reported() {
        let (run, _) = start(
            cli(&["127.0.0.1:8080", "a.test"]),
            "bubblewrap",
            Arc::default(),
        )
        .unwrap();
        assert_eq!(run.private_ips, ["127.0.0.1:8080"]);
    }

    #[test]
    fn only_bubblewrap_and_seatbelt_can_be_proxied() {
        for b in ["landlock", "windows-job", "none"] {
            let e = start(cli(&["a.test"]), b, Arc::default()).err().unwrap();
            assert!(e.contains("bubblewrap") && e.contains(b), "{e}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn starts_a_tcp_endpoint_and_stops_on_drop() {
        let (run, ep) = start(cli(&["a.test"]), "seatbelt", Arc::default()).unwrap();
        let EgressEndpoint::Tcp { port, .. } = ep else {
            panic!("tcp expected")
        };
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        drop(run);
    }
}
