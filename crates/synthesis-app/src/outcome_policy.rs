//! `synt.orchestrator.*` settings as the store's [`OutcomePolicy`]: the gate and the ceilings the
//! store itself enforces when an outcome is created or opened.

use synthesis_session::outcome::OutcomePolicy;
use synthesis_settings::OrchestratorSettings;

/// Id of the integrity disclosure below: bump it when the text changes, so a surface that stored
/// "the person saw it" knows to show it again.
pub const DISCLOSURE_ID: &str = "integrity-v1";

/// Plan 3b item 6, shown at enable time. The owner's `orchestrator.enable` reply carries it
/// (`disclosure: {id, text}`) and a surface renders that text; it never words it itself.
pub const INTEGRITY_DISCLOSURE: &str = "Outcome state is protected from agent attempts by the OS sandbox (platform dependent; native Windows refuses writable attempts). Approvals and events carry HMACs under a per-user key stored next to the data: they detect corruption, old binaries and writers that cannot read the key; they do not detect or prevent changes by any program running as the same user outside the sandbox, and they do not detect a cut-off tail of events. User and managed hooks still run on the host, outside the sandbox. There is no person-present signature yet: a program running as you outside the sandbox can read the key and forge an approval, an event or evidence.";

/// The managed layer already won in `o`; a fail-closed managed value arrives as `enabled = false`.
/// The refusal shown instead of an enable step when this build's gate is closed (`None`: open).
pub fn gate_refusal() -> Option<&'static str> {
    (!synthesis_orchestrator::enable_gate_open()).then_some(synthesis_orchestrator::GATE_REFUSAL)
}

pub fn outcome_policy(o: &OrchestratorSettings) -> OutcomePolicy {
    let mut p = OutcomePolicy::new(
        o.enabled && !o.fail_closed,
        o.max_parallel_workers,
        o.max_attempts_per_work_item,
        o.decision_expiry_hours,
    );
    p.max_cost_usd = o.max_cost_usd;
    p.attempt_max_cost_usd = o.attempt_max_cost_usd;
    p.attempt_max_wall_secs = o.attempt_max_wall_secs;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_disclosure_is_the_plan_text() {
        let plan = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/architecture/outcome-orchestration-plan.md"
        ))
        .unwrap();
        assert!(
            plan.contains(INTEGRITY_DISCLOSURE),
            "plan 3b item 6 differs"
        );
    }

    /// Changing the text without a new `DISCLOSURE_ID` would let a surface keep "the person saw it":
    /// this fails until the id is bumped and the pinned pair is updated with it.
    #[test]
    fn a_changed_disclosure_text_needs_a_new_disclosure_id() {
        use sha2::{Digest, Sha256};
        let hash: String = Sha256::digest(INTEGRITY_DISCLOSURE.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            (DISCLOSURE_ID, hash.as_str()),
            (
                "integrity-v1",
                "ee5ce515ec13695d334e5c8639f6c588d29ded353b8aacf50e72dc19e4c47bf0"
            ),
            "bump DISCLOSURE_ID, then pin the new pair here"
        );
    }

    #[test]
    fn off_by_default_and_ceilings_follow_the_settings() {
        let d = outcome_policy(&OrchestratorSettings::default());
        assert!(!d.enabled);
        assert_eq!(
            (d.max_parallel_workers, d.max_attempts_per_work_item),
            (2, 3)
        );
        let on = OrchestratorSettings {
            enabled: true,
            max_cost_usd: Some(7.0),
            attempt_max_wall_secs: Some(60),
            ..OrchestratorSettings::default()
        };
        let p = outcome_policy(&on);
        assert!(p.enabled && p.max_cost_usd == Some(7.0) && p.attempt_max_wall_secs == Some(60));
        let locked = OrchestratorSettings {
            fail_closed: true,
            ..on
        };
        assert!(!outcome_policy(&locked).enabled);
    }
}
