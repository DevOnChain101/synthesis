use std::fmt;

#[derive(Debug)]
pub enum BrowserError {
    /// No usable Chrome-family binary. Synthesis never downloads one.
    NotFound(String),
    Launch(String),
    /// The browser answered a CDP command with an error.
    Cdp {
        method: String,
        message: String,
    },
    /// The DevTools connection is gone (browser crashed or was closed).
    Closed,
    Timeout(String),
    /// Blocked by the URL policy before any request was made.
    Policy(String),
    /// The ref is unknown or its element no longer exists; take a new snapshot.
    StaleRef(String),
    Page(String),
    /// A secret or login request refused by its own checks (origin binding, field type, ...).
    Refused(String),
    /// Events were lost under load, so the URL policy could not be enforced: the pages were
    /// destroyed. Launch a new browser.
    Compromised,
    Io(String),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(m) => write!(f, "browser not found: {m}"),
            Self::Launch(m) => write!(f, "browser launch failed: {m}"),
            Self::Cdp { method, message } => write!(f, "{method} failed: {message}"),
            Self::Closed => f.write_str("browser connection closed"),
            Self::Timeout(m) => write!(f, "timed out: {m}"),
            Self::Policy(m) => write!(f, "blocked by URL policy: {m}"),
            Self::StaleRef(r) => write!(f, "ref {r} is unknown or stale; take a new snapshot"),
            Self::Page(m) => write!(f, "page error: {m}"),
            Self::Refused(m) => write!(f, "refused: {m}"),
            Self::Compromised => f.write_str(
                "browser session was torn down after event loss (policy could not be enforced); launch a new browser",
            ),
            Self::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for BrowserError {}

impl From<std::io::Error> for BrowserError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BrowserError>;
