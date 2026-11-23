//! The owner-side gateway (ADR-019 section 1): a pure default-deny table `(principal, action) ->
//! allow | deny` for the effects the orchestrator performs, and the audit payload every decision
//! writes (`policy.decision`: ids and codes only, never text from a request).
//!
//! The principal comes only from the attached link (the surface of an authenticated endpoint
//! connection, or the read-only stdio engine), never from request params or content. A worker has
//! no channel to the owner, so [`Principal::Attempt`] exists only to fail closed if one is ever
//! wired by mistake. Ownership and correctness, not a security boundary against a same-user
//! process that holds `run/orch.key` (it can attach as any surface).

use serde_json::{Value, json};
use synthesis_session::outcome::Surface;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    /// A person's surface, as the endpoint link attached.
    User(Surface),
    /// The owner process itself.
    Orchestrator,
    /// The stdio engine: reads only.
    Engine,
    /// A worker attempt: never allowed.
    Attempt,
}

impl Principal {
    pub fn label(self) -> String {
        match self {
            Principal::User(s) => format!(
                "user:{}",
                serde_json::to_value(s)
                    .map_or_else(|_| "?".into(), |v| v.as_str().unwrap_or("?").to_string())
            ),
            Principal::Orchestrator => "orchestrator".into(),
            Principal::Engine => "engine".into(),
            Principal::Attempt => "attempt".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    CreateOutcome,
    Enable,
    ApproveContract,
    Start,
    Pause,
    Resume,
    Cancel,
    Ack,
    ResolveDecision,
    /// A person's revised contract (an `edit_scope` follow-up, B4): stored unapproved.
    ReviseContract,
    PlanInstall,
    Notify,
    /// Starts a model planning run (B7 uses it): the owner only; nothing calls it yet.
    SpawnPlanner,
    /// The person's line to a work item's worker (S1 4.6): a person only, never the conductor,
    /// a worker or the read-only engine.
    MessageWorker,
}

impl Action {
    pub const ALL: [Action; 14] = [
        Action::CreateOutcome,
        Action::Enable,
        Action::ApproveContract,
        Action::Start,
        Action::Pause,
        Action::Resume,
        Action::Cancel,
        Action::Ack,
        Action::ResolveDecision,
        Action::ReviseContract,
        Action::PlanInstall,
        Action::Notify,
        Action::SpawnPlanner,
        Action::MessageWorker,
    ];

    /// The command kind (or call name) of the action.
    pub fn name(self) -> &'static str {
        match self {
            Action::CreateOutcome => "outcome.create",
            Action::Enable => "orchestrator.enable",
            Action::ApproveContract => "outcome.contract.approve",
            Action::Start => "outcome.start",
            Action::Pause => "outcome.pause",
            Action::Resume => "outcome.resume",
            Action::Cancel => "outcome.cancel",
            Action::Ack => "outcome.ack",
            Action::ResolveDecision => "decision.resolve",
            Action::ReviseContract => "outcome.contract.revise",
            Action::PlanInstall => "plan.install",
            Action::Notify => "notify",
            Action::SpawnPlanner => "planner.spawn",
            Action::MessageWorker => "attempt.message",
        }
    }

    /// `None` for a name the table does not know (the caller denies it).
    pub fn parse(name: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.name() == name)
    }

    fn by_person(self) -> bool {
        !matches!(
            self,
            Action::PlanInstall | Action::Notify | Action::SpawnPlanner
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub allow: bool,
    /// A fixed code, never request text.
    pub reason: &'static str,
}

const fn yes(reason: &'static str) -> Verdict {
    Verdict {
        allow: true,
        reason,
    }
}

const fn no(reason: &'static str) -> Verdict {
    Verdict {
        allow: false,
        reason,
    }
}

/// The table. `None` is an action nobody named in it: denied.
pub fn authorize(p: Principal, a: Option<Action>) -> Verdict {
    let Some(a) = a else {
        return no("unknown_action");
    };
    match p {
        Principal::Attempt => no("attempt_principal"),
        Principal::Engine => no("read_only_principal"),
        Principal::User(_) if a.by_person() => yes("person_action"),
        Principal::Orchestrator if !a.by_person() => yes("owner_action"),
        _ => no("wrong_principal"),
    }
}

/// The `policy.decision` payload of one call: the principal and action are table values, the
/// reason a fixed code.
pub fn audit(p: Principal, a: Option<Action>) -> Value {
    let v = authorize(p, a);
    json!({"action": a.map_or("unknown", Action::name),
           "result": if v.allow { "allow" } else { "deny" },
           "detail": {"principal": p.label(), "reason": v.reason}})
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURFACES: [Surface; 3] = [Surface::Cli, Surface::Tui, Surface::Superagent];

    fn allowed(p: Principal, a: Action) -> bool {
        authorize(p, Some(a)).allow
    }

    #[test]
    fn unknown_action_denied() {
        assert_eq!(Action::parse("rm.rf"), None);
        for p in [Principal::User(Surface::Cli), Principal::Orchestrator] {
            assert_eq!(authorize(p, None).reason, "unknown_action");
        }
        assert_eq!(audit(Principal::Orchestrator, None)["action"], "unknown");
    }

    #[test]
    fn attempt_principal_always_denied() {
        for a in Action::ALL {
            assert!(!allowed(Principal::Attempt, a), "{a:?}");
        }
    }

    #[test]
    fn engine_is_denied_everything_mutating() {
        for a in Action::ALL {
            assert!(!allowed(Principal::Engine, a), "{a:?}");
        }
    }

    #[test]
    fn a_person_may_only_do_person_actions() {
        for s in SURFACES {
            for a in Action::ALL {
                let want = a.by_person();
                assert_eq!(allowed(Principal::User(s), a), want, "{s:?} {a:?}");
            }
        }
    }

    #[test]
    fn the_orchestrator_cannot_resolve_approve_or_start() {
        for a in Action::ALL {
            let want = matches!(
                a,
                Action::PlanInstall | Action::Notify | Action::SpawnPlanner
            );
            assert_eq!(allowed(Principal::Orchestrator, a), want, "{a:?}");
        }
    }

    #[test]
    fn only_the_orchestrator_may_spawn_a_planner() {
        for s in SURFACES {
            assert_eq!(
                authorize(Principal::User(s), Some(Action::SpawnPlanner)).reason,
                "wrong_principal"
            );
        }
        for p in [Principal::Engine, Principal::Attempt] {
            assert!(!allowed(p, Action::SpawnPlanner), "{p:?}");
        }
        let v = authorize(Principal::Orchestrator, Some(Action::SpawnPlanner));
        assert!(v.allow && v.reason == "owner_action");
    }

    #[test]
    fn a_revision_is_a_person_action_only() {
        for s in SURFACES {
            assert!(allowed(Principal::User(s), Action::ReviseContract));
        }
        for p in [
            Principal::Engine,
            Principal::Attempt,
            Principal::Orchestrator,
        ] {
            assert!(!allowed(p, Action::ReviseContract), "{p:?}");
        }
    }

    #[test]
    fn a_worker_message_is_a_person_action_only() {
        for s in SURFACES {
            assert!(allowed(Principal::User(s), Action::MessageWorker));
        }
        for p in [
            Principal::Engine,
            Principal::Attempt,
            Principal::Orchestrator,
        ] {
            assert!(!allowed(p, Action::MessageWorker), "{p:?}");
        }
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        for a in Action::ALL {
            assert_eq!(Action::parse(a.name()), Some(a));
        }
    }

    #[test]
    fn the_audit_carries_codes_only() {
        let v = audit(
            Principal::User(Surface::Tui),
            Action::parse("decision.resolve"),
        );
        assert_eq!(
            v,
            json!({"action": "decision.resolve", "result": "allow",
                   "detail": {"principal": "user:tui", "reason": "person_action"}})
        );
    }
}
