//! What the host may read for the conductor. The window implements it over its engines and the outcome
//! client; this module has no store access. Rows carry raw text: `digest` cleans it before a model sees it.

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    /// `s<N>`, never reused in a window's life.
    pub handle: String,
    pub sid: String,
    pub title: String,
    pub state: String,
    pub model: String,
    pub mode: String,
    pub cwd_label: String,
    pub spent_usd: f64,
    pub task_summary: String,
    pub live: bool,
}

/// What `conductor_read_digest` may say about a session (conductor.md 3.6).
#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    pub last_reply: String,
    pub last_tools: Vec<String>,
    /// `(kind, tool name)` of a pending ask; its input is never given.
    pub pending_ask: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeRow {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub state: String,
    pub needs_you: u32,
    pub spent_usd: f64,
    pub cap_usd: f64,
}

/// A work item: typed fields only, no brief, worker or diff text.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub id: String,
    pub outcome_id: String,
    pub state: String,
    pub attempts: u32,
    pub needs_you: bool,
    pub spent_usd: f64,
}

/// Read-only access to what the conductor may know. Never a write path.
pub trait ConductorReads: Send + Sync {
    /// Own sessions only (this window's), live or ended.
    fn sessions(&self) -> Vec<SessionRow>;
    fn session_report(&self, handle: &str) -> Option<SessionReport>;
    fn outcomes(&self, state: Option<&str>) -> Vec<OutcomeRow>;
    fn outcome(&self, id: &str) -> Option<OutcomeRow>;
    fn item(&self, id: &str) -> Option<ItemRow>;
    /// The outcome's report, as the worker wrote it (untrusted).
    fn report(&self, outcome_id: &str) -> Option<String>;
}
