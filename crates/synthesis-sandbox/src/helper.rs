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

use crate::{NetworkPolicy, SandboxError, SandboxPolicy, validate};

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
