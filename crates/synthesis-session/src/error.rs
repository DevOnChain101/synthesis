use std::path::PathBuf;

use synthesis_core::InvalidTransition;

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    InvalidTransition(InvalidTransition),
    NotFound(String),
    AlreadyExists(String),
    /// Another process (or writer) owns this session.
    Locked(String),
    SchemaTooNew {
        found: i64,
        supported: i64,
    },
    TooLarge {
        what: &'static str,
        size: usize,
        max: usize,
    },
    InvalidId(String),
    /// A configured per-session cap (events, event bytes, artifact bytes) was reached.
    QuotaExceeded {
        what: &'static str,
        limit: u64,
    },
    /// The Node JSONL changed since our last mirror write; re-import before resuming (spec C.3).
    TranscriptChanged(PathBuf),
    OutsideWorkspace(PathBuf),
    Invalid(String),
    /// The database is below schema v5: outcomes were never enabled here.
    OutcomesNotEnabled,
    /// A compare-and-swap on an aggregate's `revision` lost: reload and decide again.
    RevisionConflict {
        aggregate: &'static str,
        id: String,
        expected: i64,
        actual: i64,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "sqlite: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::InvalidTransition(e) => write!(f, "{e}"),
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::AlreadyExists(s) => write!(f, "already exists: {s}"),
            Self::Locked(s) => write!(f, "session {s} is open for writing in another process"),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "database schema v{found} is newer than this build supports (v{supported})"
            ),
            Self::TooLarge { what, size, max } => {
                write!(f, "{what} is {size} bytes, over the {max} byte cap")
            }
            Self::InvalidId(s) => write!(f, "invalid id {s:?}"),
            Self::QuotaExceeded { what, limit } => {
                write!(f, "session quota reached: {what} (limit {limit})")
            }
            Self::TranscriptChanged(p) => write!(
                f,
                "{} changed since the last mirror write; re-import it first",
                p.display()
            ),
            Self::OutsideWorkspace(p) => write!(f, "{} is outside the workspace", p.display()),
            Self::Invalid(s) => write!(f, "{s}"),
            Self::OutcomesNotEnabled => write!(
                f,
                "outcome orchestration is not enabled for this store (run `synt outcome enable`)"
            ),
            Self::RevisionConflict {
                aggregate,
                id,
                expected,
                actual,
            } => write!(
                f,
                "{aggregate} {id} is at revision {actual}, not {expected}: reload and retry"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<InvalidTransition> for Error {
    fn from(e: InvalidTransition) -> Self {
        Self::InvalidTransition(e)
    }
}
