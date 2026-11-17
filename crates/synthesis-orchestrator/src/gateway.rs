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
    /// Reserved (B4): denied until it lands.
    ReviseContract,
    PlanInstall,
    Notify,
    /// Reserved (model planner): denied until it lands.
    SpawnPlanner,
}

impl Action {
    pub const ALL: [Action; 13] = [
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
    if matches!(a, Action::ReviseContract | Action::SpawnPlanner) {
        return no("not_available");
    }
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
                let want = a.by_person() && !matches!(a, Action::ReviseContract);
                assert_eq!(allowed(Principal::User(s), a), want, "{s:?} {a:?}");
            }
        }
    }

    #[test]
    fn the_orchestrator_cannot_resolve_approve_or_start() {
        for a in Action::ALL {
            let want = matches!(a, Action::PlanInstall | Action::Notify);
            assert_eq!(allowed(Principal::Orchestrator, a), want, "{a:?}");
        }
    }

    #[test]
    fn reserved_actions_are_denied_for_everyone() {
        for a in [Action::ReviseContract, Action::SpawnPlanner] {
            for p in [
                Principal::User(Surface::Cli),
                Principal::Orchestrator,
                Principal::Engine,
                Principal::Attempt,
            ] {
                assert_eq!(authorize(p, Some(a)).reason, "not_available");
            }
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
