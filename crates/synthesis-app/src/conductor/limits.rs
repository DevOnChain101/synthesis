//! Rate limits (super-agent-s1.md 4.7). The numbers are code constants: no setting raises them.

use std::collections::HashMap;

pub const MAX_CALLS_PER_TURN: u32 = 12;
pub const MAX_PROPOSALS_PER_TURN: u32 = 3;
pub const MAX_GOALS_PER_TURN: u32 = 1;
pub const MAX_OPEN_PROPOSALS: usize = 10;
/// Sends to one own session per window.
pub const MAX_SENDS_PER_TARGET: usize = 10;
pub const SEND_WINDOW_SECS: u64 = 10 * 60;
/// A shown proposal expires after this long (4.3).
pub const PROPOSAL_TTL_SECS: u64 = 30 * 60;

/// What one conductor turn (one line of the person) has used.
#[derive(Debug, Default, Clone, Copy)]
pub struct Turn {
    pub calls: u32,
    pub proposals: u32,
    pub goals: u32,
}

/// When the person's sends to each own session happened, to cap a flood through the conductor (T4).
#[derive(Debug, Default)]
pub struct SendLog(HashMap<String, Vec<u64>>);

impl SendLog {
    pub fn allows(&mut self, sid: &str, now: u64) -> bool {
        let v = self.0.entry(sid.to_string()).or_default();
        v.retain(|t| now.saturating_sub(*t) < SEND_WINDOW_SECS);
        v.len() < MAX_SENDS_PER_TARGET
    }

    pub fn record(&mut self, sid: &str, now: u64) {
        self.0.entry(sid.to_string()).or_default().push(now);
    }
}
