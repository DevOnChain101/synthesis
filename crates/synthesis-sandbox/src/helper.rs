//! Wire format and argv parsing for the Landlock re-exec helper (`synthesis-sandbox-exec`).
//!
//! Invocation: `synthesis-sandbox-exec '<policy json>' -- <program> [args...]`. The policy is argv,
//! not stdin, because stdin belongs to the target command. The JSON is strict: unknown fields
//! and any `version` other than [`VERSION`] are rejected (fail closed). Env passthrough and
//! `required` are not part of the wire format; the wrapping side already applied them.
//! Portable on purpose so it is unit-tested on every OS; only the helper binary is Linux-only.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{NetworkPolicy, ResourceLimits, SandboxError, SandboxPolicy, validate};

pub const HELPER_NAME: &str = "synthesis-sandbox-exec";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Net {
    Deny,
    Allow,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u32,
    readable_roots: Vec<PathBuf>,
    writable_roots: Vec<PathBuf>,
    deny_paths: Vec<PathBuf>,
    network: Net,
    allow_exec: bool,
}

pub fn encode_policy(p: &SandboxPolicy) -> Result<String, SandboxError> {
    validate(p)?;
    if matches!(
        p.network,
        NetworkPolicy::Loopback | NetworkPolicy::Proxied(_)
    ) {
        return Err(SandboxError::Unsupported(
            "network Loopback and Proxied cannot be expressed to the Landlock helper".into(),
        ));
    }
    let w = Wire {
        version: VERSION,
        readable_roots: p.readable_roots.clone(),
        writable_roots: p.writable_roots.clone(),
        deny_paths: p.deny_paths.clone(),
        network: if p.network == NetworkPolicy::Allow {
            Net::Allow
        } else {
            Net::Deny
        },
        allow_exec: p.allow_exec,
    };
    serde_json::to_string(&w).map_err(|e| SandboxError::InvalidPolicy(e.to_string()))
}

pub fn decode_policy(s: &str) -> Result<SandboxPolicy, SandboxError> {
    let bad = |m: String| SandboxError::InvalidPolicy(m);
    let w: Wire = serde_json::from_str(s).map_err(|e| bad(e.to_string()))?;
    if w.version != VERSION {
        return Err(bad(format!("unsupported policy version {}", w.version)));
    }
    let p = SandboxPolicy {
        readable_roots: w.readable_roots,
        writable_roots: w.writable_roots,
        deny_paths: w.deny_paths,
        network: match w.network {
            Net::Deny => NetworkPolicy::Deny,
            Net::Allow => NetworkPolicy::Allow,
        },
        allow_exec: w.allow_exec,
        ..Default::default()
    };
    validate(&p)?;
    Ok(p)
}

/// Value of the helper's leading `--limits` argument: `core=0` always, then the set limits.
pub fn limits_arg(l: &ResourceLimits) -> String {
    let mut v = vec!["core=0".to_string()];
    for (k, x) in [
        ("fsize", l.fsize),
        ("nofile", l.nofile),
        ("nproc", l.nproc),
        ("cpu", l.cpu_secs),
        ("mem", l.memory),
        ("tasks", l.tasks),
    ] {
        v.extend(x.map(|x| format!("{k}={x}")));
    }
    v.join(",")
}

/// Strict inverse of [`limits_arg`] (`disk_quota` is not part of it). Unknown keys, repeated
/// keys, a `core` other than 0 and non-decimal numbers are errors.
pub fn parse_limits_arg(s: &str) -> Result<ResourceLimits, SandboxError> {
    let bad = |m: String| SandboxError::InvalidPolicy(format!("--limits: {m}"));
    let mut l = ResourceLimits {
        fsize: None,
        memory: None,
        tasks: None,
        disk_quota: None,
        ..Default::default()
    };
    let mut seen: Vec<&str> = vec![];
    for part in s.split(',') {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| bad(format!("{part:?}")))?;
        let n: u64 = v
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| v.parse().ok())
            .flatten()
            .ok_or_else(|| bad(format!("{part:?}")))?;
        if seen.contains(&k) {
            return Err(bad(format!("{k} given twice")));
        }
        seen.push(k);
        match k {
            "core" if n == 0 => {}
            "fsize" => l.fsize = Some(n),
            "nofile" => l.nofile = Some(n),
            "nproc" => l.nproc = Some(n),
            "cpu" => l.cpu_secs = Some(n),
            "mem" => l.memory = Some(n),
            "tasks" => l.tasks = Some(n),
            _ => return Err(bad(format!("{part:?}"))),
        }
    }
    Ok(l)
}

/// argv without the executable name -> (policy, program, program args).
pub fn parse_helper_args(
    args: &[OsString],
) -> Result<(SandboxPolicy, OsString, Vec<OsString>), SandboxError> {
    let usage = || {
        SandboxError::InvalidPolicy(format!(
            "usage: {HELPER_NAME} <policy-json> -- <program> [args...]"
        ))
    };
    let [json, sep, program, rest @ ..] = args else {
        return Err(usage());
    };
    if sep != "--" {
        return Err(usage());
    }
    let json = json
        .to_str()
        .ok_or_else(|| SandboxError::InvalidPolicy("policy is not UTF-8".into()))?;
    Ok((decode_policy(json)?, program.clone(), rest.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // POSIX-looking test paths get a drive letter on Windows so they are absolute there too.
    fn p(s: &str) -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            format!("C:{s}")
        } else {
            s.into()
        })
    }

    fn pol() -> SandboxPolicy {
        SandboxPolicy {
            readable_roots: vec![p("/usr")],
            writable_roots: vec![p("/work")],
            deny_paths: vec![p("/home/u/.ssh")],
            network: NetworkPolicy::Allow,
            allow_exec: true,
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip() {
        let p = decode_policy(&encode_policy(&pol()).unwrap()).unwrap();
        assert_eq!(p.writable_roots, pol().writable_roots);
        assert_eq!(p.deny_paths, pol().deny_paths);
        assert_eq!(p.network, NetworkPolicy::Allow);
    }

    #[test]
    fn strict_parsing() {
        let ok = encode_policy(&pol()).unwrap();
        let with = |from: &str, to: &str| ok.replacen(from, to, 1);
        for bad in [
            with("\"version\":1", "\"version\":2"),
            with("\"version\":1", "\"version\":1,\"extra\":true"),
            with("\"allow\"", "\"Proxied\""),
            with(&serde_json::to_string(&p("/work")).unwrap(), "\"rel\""),
            "{}".into(),
            String::new(),
        ] {
            assert!(
                matches!(decode_policy(&bad), Err(SandboxError::InvalidPolicy(_))),
                "{bad}"
            );
        }
        let hosts = SandboxPolicy {
            network: NetworkPolicy::Proxied(crate::EgressEndpoint::Unix(p("/r/e.sock"))),
            ..Default::default()
        };
        assert!(encode_policy(&hosts).is_err());
    }

    #[test]
    fn limits_arg_roundtrip_and_strict() {
        let l = ResourceLimits {
            nofile: Some(256),
            cpu_secs: Some(60),
            disk_quota: None,
            ..Default::default()
        };
        let a = limits_arg(&l);
        assert_eq!(
            a,
            "core=0,fsize=4294967296,nofile=256,cpu=60,mem=8589934592,tasks=1024"
        );
        assert_eq!(parse_limits_arg(&a).unwrap(), l);
        for bad in [
            "",
            "core=1",
            "fsize",
            "fsize=+1",
            "fsize=-1",
            "x=1",
            "cpu=1,cpu=2",
            "cpu=1e3",
        ] {
            assert!(parse_limits_arg(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn argv_shape() {
        let json: OsString = encode_policy(&pol()).unwrap().into();
        let a = |v: &[&OsString]| v.iter().map(|s| (*s).clone()).collect::<Vec<_>>();
        let (dd, sh, c, x) = (&"--".into(), &"sh".into(), &"-c".into(), &"true".into());
        let (_, prog, rest) = parse_helper_args(&a(&[&json, dd, sh, c, x])).unwrap();
        assert_eq!(
            (prog, rest),
            ("sh".into(), vec!["-c".into(), "true".into()])
        );
        assert!(parse_helper_args(&a(&[&json, sh, c])).is_err());
        assert!(parse_helper_args(&a(&[&json, dd])).is_err());
        assert!(parse_helper_args(&[]).is_err());
    }
}
