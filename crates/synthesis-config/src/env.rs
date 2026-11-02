use std::collections::BTreeMap;

/// Set in the environment of every child process the core starts (see [`Env::child_env`]).
pub const AGENT_SESSION_VAR: &str = "SYNT_AGENT_SESSION";

/// Explicit environment snapshot. Library code never reads the process env
/// except through [`Env::from_process`].
///
/// Layering and credentials read only `SYNT_*` names. Other names are reachable
/// solely through an explicit `{env:NAME}` reference written by the user in a
/// provider entry (docs/specs/config.md §8).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Env(BTreeMap<String, String>);

// Values can be secrets (and any name can hold one): Debug lists names only.
impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Env")
            .field(&self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Env {
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    /// Snapshot of the real process environment (non-UTF-8 is converted lossily).
    pub fn from_process() -> Self {
        Self::from_pairs(std::env::vars_os().map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        }))
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// JS `env.X || fallback` semantics: an empty value counts as unset.
    pub fn get_nonempty(&self, name: &str) -> Option<&str> {
        self.get(name).filter(|v| !v.is_empty())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The canonical environment for child processes (Bash, hooks, `!`, MCP stdio): an
    /// ALLOWLIST. Passes only the standard names in [`CHILD_ENV_ALLOWLIST`], `LC_*`, `XDG_*`
    /// and `SYNT_*` names that are not secret-shaped (see [`is_secret_shaped`]). Everything
    /// else (`AWS_*`, `GITHUB_TOKEN`, `ANTHROPIC_API_KEY`, ...) is dropped. The only ways to
    /// opt another variable in are `{env:NAME}` in config and an explicit per-server env map.
    pub fn child_env(&self) -> Env {
        let mut env = Env(self
            .0
            .iter()
            .filter(|(k, _)| {
                let k = k.as_str();
                if let Some(rest) = k.strip_prefix("SYNT_") {
                    return !is_secret_shaped(rest);
                }
                CHILD_ENV_ALLOWLIST.contains(&k) || k.starts_with("LC_") || k.starts_with("XDG_")
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect());
        // Output only (never read as a setting): commands the agent starts can tell they are not
        // the person, so `synt memory` refuses to approve or delete from there.
        env.0.insert(AGENT_SESSION_VAR.into(), "1".into());
        env
    }

    #[deprecated(note = "denylist; use Env::child_env (allowlist)")]
    pub fn without_secrets(&self) -> Env {
        Env(self
            .0
            .iter()
            .filter(|(k, _)| !is_secret_env_name(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}

/// `^SYNT_(?:[A-Z0-9]+_)*(?:API_KEY|AUTH_TOKEN|TOKEN)$`
pub fn is_secret_env_name(name: &str) -> bool {
    let Some(mut rest) = name.strip_prefix("SYNT_") else {
        return false;
    };
    loop {
        if matches!(rest, "API_KEY" | "AUTH_TOKEN" | "TOKEN") {
            return true;
        }
        let Some((seg, tail)) = rest.split_once('_') else {
            return false;
        };
        if seg.is_empty()
            || !seg
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return false;
        }
        rest = tail;
    }
}

/// Standard, non-secret names passed to children (`LC_*` and `XDG_*` by prefix).
pub const CHILD_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "Path",
    "HOME",
    "USERPROFILE",
    // Windows profile dirs: npm/npx and Node's os.homedir() (e.g. `mcp-remote`'s ~/.mcp-auth) need them
    "APPDATA",
    "LOCALAPPDATA",
    "HOMEDRIVE",
    "HOMEPATH",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SystemRoot",
    "SYSTEMROOT",
    "ComSpec",
    "PATHEXT",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "LANG",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CI",
];

/// True when any `_`-separated segment of the name is KEY, TOKEN, SECRET, PASSWORD, PASSWD,
/// CREDENTIAL(S), AUTH, PRIVATE, SESSION or COOKIE (case-insensitive).
pub fn is_secret_shaped(name: &str) -> bool {
    name.split('_').any(|seg| {
        [
            "KEY",
            "TOKEN",
            "SECRET",
            "PASSWORD",
            "PASSWD",
            "CREDENTIAL",
            "CREDENTIALS",
            "AUTH",
            "PRIVATE",
            "SESSION",
            "COOKIE",
        ]
        .iter()
        .any(|s| seg.eq_ignore_ascii_case(s))
    })
}
