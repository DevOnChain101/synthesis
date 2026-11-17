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
