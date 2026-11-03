use std::fmt;

/// Why a desktop call did not do its job. `kind()` is the stable code in tool `meta.error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopError {
    /// No binary, no pin match, no display: nothing can be controlled here.
    Unavailable(String),
    /// Policy said no (hard-deny list, mode, scope, a window outside the app's process tree).
    Refused(String),
    Timeout(String),
    /// The ref is unknown, superseded or no longer matches the UI: take a new snapshot.
    StaleRef(String),
    /// agent-desktop answered `ok:false`. `retry_safe` is its `disposition.retry == "safe"`.
    Tool {
        code: String,
        message: String,
        retry_safe: bool,
    },
    /// The output was not a valid envelope, too long, or of an unsupported version.
    Protocol(String),
    RateLimited(String),
    NotFound(String),
}

impl DesktopError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "desktop_unavailable",
            Self::Refused(_) => "refused",
            Self::Timeout(_) => "timeout",
            Self::StaleRef(_) => "stale_ref",
            Self::Tool { code, .. } if code == "AMBIGUOUS_TARGET" => "ambiguous_target",
            Self::Tool { code, .. } if code == "STALE_REF" => "stale_ref",
            Self::Tool { .. } => "desktop_error",
            Self::Protocol(_) => "protocol_error",
            Self::RateLimited(_) => "rate_limited",
            Self::NotFound(_) => "not_found",
        }
    }
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m)
            | Self::Refused(m)
            | Self::Timeout(m)
            | Self::StaleRef(m)
            | Self::Protocol(m)
            | Self::RateLimited(m)
            | Self::NotFound(m) => f.write_str(m),
            Self::Tool {
                code,
                message,
                retry_safe,
            } => write!(
                f,
                "{code}: {message}{}",
                if *retry_safe {
                    " (nothing was delivered; safe to retry after a new snapshot)"
                } else {
                    " (input may already have been delivered: take a snapshot and check before repeating)"
                }
            ),
        }
    }
}

impl std::error::Error for DesktopError {}

pub type Result<T> = std::result::Result<T, DesktopError>;
