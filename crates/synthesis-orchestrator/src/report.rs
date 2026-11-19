//! The report of a read-only goal's attempt (S1 section 2.3): the worker's final message. P2a
//! only names it (`report:sha256:<hex>` is the attempt's subject); the citation check and the
//! person's review of it are P2b's. Nothing here marks anything verified.

use sha2::{Digest, Sha256};
use synthesis_core::outcome::ContractKind;
use synthesis_session::outcome::OutcomeWriter;
use synthesis_session::{Result, Store};

/// What the worker said last, as written to its session. Model text: data, never instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReport {
    pub text: String,
    pub sha256: String,
}

impl AttemptReport {
    /// `attempts.subject_commit` of a read-only attempt: no commit exists, the report is the subject.
    pub fn subject(&self) -> String {
        format!("report:sha256:{}", self.sha256)
    }
}

/// The final assistant message of `session_id`. P2b: show exactly the hashed text, recheck the
/// hash when it is displayed, and treat it as untrusted (strip terminal escapes, no links, never
/// pass it to a model).
///
/// The final assistant message of `session_id`, `None` when it wrote none (or the store cannot say).
pub fn attempt_report(store: &Store, session_id: &str) -> Option<AttemptReport> {
    let text = crate::planrun::read_session(store, session_id).0?;
    let sha256 = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(AttemptReport { text, sha256 })
}

/// Is the outcome a read-only goal? Read from the stored contract of the outcome's current version
/// (never from a flag a caller passes), so what runs is what the person approved.
pub fn is_read_only(w: &OutcomeWriter) -> Result<bool> {
    let v = w.get_outcome(w.id())?.contract_version;
    Ok(w.get_contract(w.id(), v)?.0.kind == ContractKind::ReadOnly)
}
