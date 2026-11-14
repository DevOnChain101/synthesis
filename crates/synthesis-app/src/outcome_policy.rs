//! `synt.orchestrator.*` settings as the store's [`OutcomePolicy`]: the gate and the ceilings the
//! store itself enforces when an outcome is created or opened.

use synthesis_session::outcome::OutcomePolicy;
use synthesis_settings::OrchestratorSettings;

/// The managed layer already won in `o`; a fail-closed managed value arrives as `enabled = false`.
pub fn outcome_policy(o: &OrchestratorSettings) -> OutcomePolicy {
    OutcomePolicy {
        enabled: o.enabled && !o.fail_closed,
        max_parallel_workers: o.max_parallel_workers,
        max_attempts_per_work_item: o.max_attempts_per_work_item,
        max_cost_usd: o.max_cost_usd,
        attempt_max_cost_usd: o.attempt_max_cost_usd,
        attempt_max_wall_secs: o.attempt_max_wall_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
