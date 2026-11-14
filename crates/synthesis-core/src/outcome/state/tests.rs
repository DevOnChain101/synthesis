use super::*;

// Independent copy of the tables (written out pair by pair), so a typo in the match arms shows up.
const OUTCOME: &[(OutcomeState, OutcomeState)] = {
    use OutcomeState::*;
    &[
        (Draft, ContractPending),
        (ContractPending, Ready),
        (Ready, Running),
        (Running, Verifying),
        (Running, Blocked),
        (Running, Failed),
        (Blocked, Running),
        (Verifying, Verified),
        (Verifying, ReworkRequired),
        (ReworkRequired, Running),
        (Verified, Completed),
        (Draft, Cancelled),
        (ContractPending, Cancelled),
        (Ready, Cancelled),
        (Running, Cancelled),
        (Blocked, Cancelled),
        (Verifying, Cancelled),
        (ReworkRequired, Cancelled),
        (Verified, Cancelled),
    ]
};

const WORK_ITEM: &[(WorkItemState, WorkItemState)] = {
    use WorkItemState::*;
    &[
        (Created, Ready),
        (Ready, Leased),
        (Leased, Running),
        (Running, Verifying),
        (Verifying, Succeeded),
        (Running, WaitingDecision),
        (WaitingDecision, Ready),
        (Running, RetryableFailure),
        (Verifying, RetryableFailure),
        (RetryableFailure, Ready),
        (Running, TerminalFailure),
        (Verifying, TerminalFailure),
        (RetryableFailure, TerminalFailure),
        (Recovering, TerminalFailure),
        (Leased, Recovering),
        (Running, Recovering),
        (Recovering, Ready),
        (Recovering, Blocked),
        (Blocked, Ready),
        (CancelRequested, Cancelled),
        (Created, CancelRequested),
        (Ready, CancelRequested),
        (Leased, CancelRequested),
        (Running, CancelRequested),
        (Verifying, CancelRequested),
        (WaitingDecision, CancelRequested),
        (RetryableFailure, CancelRequested),
        (Recovering, CancelRequested),
        (Blocked, CancelRequested),
    ]
};

const ATTEMPT: &[(AttemptState, AttemptState)] = {
    use AttemptState::*;
    &[
        (Starting, Running),
        (Starting, Lost),
        (Starting, Cancelled),
        (Running, Finished),
        (Running, Lost),
        (Running, Cancelled),
    ]
};

const DECISION: &[(DecisionState, DecisionState)] = {
    use DecisionState::*;
    &[
        (Preparing, Pending),
        (Preparing, Invalidated),
        (Pending, Approved),
        (Pending, Rejected),
        (Pending, Expired),
        (Pending, Invalidated),
        (Pending, NeedsMoreInfo),
        (NeedsMoreInfo, Pending),
        (NeedsMoreInfo, Expired),
        (NeedsMoreInfo, Invalidated),
    ]
};

/// Every ordered pair, listed or not.
fn check_table<S: Copy + PartialEq + std::fmt::Debug>(
    all: &[S],
    expected: &[(S, S)],
    can: impl Fn(S, S) -> bool,
) {
    for &from in all {
        for &to in all {
            assert_eq!(
                can(from, to),
                expected.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
}

#[test]
fn every_pair_matches_the_written_tables() {
    check_table(&OutcomeState::ALL, OUTCOME, OutcomeState::can_transition_to);
    check_table(
        &WorkItemState::ALL,
        WORK_ITEM,
        WorkItemState::can_transition_to,
    );
    check_table(&AttemptState::ALL, ATTEMPT, AttemptState::can_transition_to);
    check_table(
        &DecisionState::ALL,
        DECISION,
        DecisionState::can_transition_to,
    );
}

#[test]
fn terminal_states_have_no_exit_and_no_self_loops() {
    fn check<S: Copy + PartialEq + std::fmt::Debug>(
        all: &[S],
        term: impl Fn(S) -> bool,
        can: impl Fn(S, S) -> bool,
    ) {
        for &a in all {
            assert!(!can(a, a), "self loop {a:?}");
            if term(a) {
                assert!(all.iter().all(|&b| !can(a, b)), "{a:?} is terminal");
            }
        }
    }
    check(
        &OutcomeState::ALL,
        OutcomeState::is_terminal,
        OutcomeState::can_transition_to,
    );
    check(
        &WorkItemState::ALL,
        WorkItemState::is_terminal,
        WorkItemState::can_transition_to,
    );
    check(
        &AttemptState::ALL,
        AttemptState::is_terminal,
        AttemptState::can_transition_to,
    );
    check(
        &DecisionState::ALL,
        DecisionState::is_terminal,
        DecisionState::can_transition_to,
    );
}

#[test]
fn only_non_terminal_outcomes_and_work_items_can_be_cancelled() {
    for s in OutcomeState::ALL {
        assert_eq!(
            s.can_transition_to(OutcomeState::Cancelled),
            !s.is_terminal(),
            "{s:?}"
        );
    }
    for s in WorkItemState::ALL {
        let ok = !s.is_terminal() && s != WorkItemState::CancelRequested;
        assert_eq!(
            s.can_transition_to(WorkItemState::CancelRequested),
            ok,
            "{s:?}"
        );
    }
}

#[test]
fn outcome_guards() {
    use OutcomeState::*;
    let none = OutcomeGuards::default();
    let all = OutcomeGuards {
        contract_approved: true,
        plan_valid: true,
        verdict_verified: true,
        delivery_ready: true,
    };
    let half = OutcomeGuards {
        contract_approved: true,
        ..none
    };
    for (from, to) in [
        (ContractPending, Ready),
        (Verifying, Verified),
        (Verified, Completed),
    ] {
        let e = from.transition(to, &none).unwrap_err();
        assert!(matches!(e.refusal, Refusal::Guard(_)), "{e}");
        assert_eq!(from.transition(to, &all), Ok(to));
    }
    // approval alone is not enough: the plan must be valid too
    assert!(ContractPending.transition(Ready, &half).is_err());
    // the verdict guard is the only way to Verified; an unrelated flag does not help
    let only_delivery = OutcomeGuards {
        delivery_ready: true,
        ..none
    };
    assert!(Verifying.transition(Verified, &only_delivery).is_err());
    // a pair outside the table is NotAllowed whatever the guards say
    let e = Draft.transition(Running, &all).unwrap_err();
    assert_eq!(e.refusal, Refusal::NotAllowed);
    assert!(e.to_string().contains("invalid transition"));
    // cancel needs no guard
    assert_eq!(Running.transition(Cancelled, &none), Ok(Cancelled));
}

#[test]
fn work_item_retry_budget_and_evidence_guards() {
    use WorkItemState::*;
    let g = |used, max| WorkItemGuards {
        retries_used: used,
        max_retries: max,
        independent_evidence: false,
    };
    assert_eq!(
        Running.transition(RetryableFailure, &g(1, 2)),
        Ok(RetryableFailure)
    );
    assert_eq!(RetryableFailure.transition(Ready, &g(1, 2)), Ok(Ready));
    // budget 2: the third failure has no retry left
    assert!(Running.transition(RetryableFailure, &g(2, 2)).is_err());
    assert!(RetryableFailure.transition(Ready, &g(2, 2)).is_err());
    assert!(Verifying.transition(RetryableFailure, &g(0, 0)).is_err());
    // terminal failure is always reachable from the failure paths
    assert_eq!(
        RetryableFailure.transition(TerminalFailure, &g(2, 2)),
        Ok(TerminalFailure)
    );
    // success needs independent evidence
    assert!(Verifying.transition(Succeeded, &g(0, 2)).is_err());
    let ev = WorkItemGuards {
        independent_evidence: true,
        ..g(0, 2)
    };
    assert_eq!(Verifying.transition(Succeeded, &ev), Ok(Succeeded));
    // Running -> Succeeded skips verification: not in the table
    assert_eq!(
        Running.transition(Succeeded, &ev).unwrap_err().refusal,
        Refusal::NotAllowed
    );
}

#[test]
fn decision_actors_and_bindings() {
    use DecisionState::*;
    let g = |actor, binding_current, expiry_reached| DecisionGuards {
        actor,
        binding_current,
        expiry_reached,
    };
    // only a person resolves
    for to in [Approved, Rejected, NeedsMoreInfo] {
        assert_eq!(Pending.transition(to, &g(Actor::User, true, false)), Ok(to));
        for who in [Actor::Orchestrator, Actor::Attempt] {
            assert!(
                Pending.transition(to, &g(who, true, false)).is_err(),
                "{who:?} {to:?}"
            );
        }
        // a stale binding or an expired decision is never resolved
        assert!(
            Pending
                .transition(to, &g(Actor::User, false, false))
                .is_err()
        );
        assert!(Pending.transition(to, &g(Actor::User, true, true)).is_err());
    }
    // expiry is the orchestrator's, and only when the time has come; never an approve
    assert_eq!(
        Pending.transition(Expired, &g(Actor::Orchestrator, true, true)),
        Ok(Expired)
    );
    assert!(
        Pending
            .transition(Expired, &g(Actor::Orchestrator, true, false))
            .is_err()
    );
    assert!(
        Pending
            .transition(Expired, &g(Actor::User, true, true))
            .is_err()
    );
    // invalidation needs a changed binding
    assert_eq!(
        Pending.transition(Invalidated, &g(Actor::Orchestrator, false, false)),
        Ok(Invalidated)
    );
    assert!(
        Pending
            .transition(Invalidated, &g(Actor::Orchestrator, true, false))
            .is_err()
    );
    assert!(
        Pending
            .transition(Invalidated, &g(Actor::Attempt, false, false))
            .is_err()
    );
    // preparing and re-opening after more info are the orchestrator's
    assert_eq!(
        Preparing.transition(Pending, &g(Actor::Orchestrator, true, false)),
        Ok(Pending)
    );
    assert_eq!(
        NeedsMoreInfo.transition(Pending, &g(Actor::Orchestrator, true, false)),
        Ok(Pending)
    );
    assert!(
        Preparing
            .transition(Pending, &g(Actor::User, true, false))
            .is_err()
    );
    // an attempt is refused everywhere
    for from in DecisionState::ALL {
        for to in DecisionState::ALL {
            for (b, x) in [(true, false), (false, false), (true, true), (false, true)] {
                assert!(
                    from.transition(to, &g(Actor::Attempt, b, x)).is_err(),
                    "{from:?}->{to:?}"
                );
            }
        }
    }
}

#[test]
fn attempt_transitions() {
    use AttemptState::*;
    assert_eq!(Starting.transition(Running), Ok(Running));
    assert_eq!(Running.transition(Finished), Ok(Finished));
    assert!(
        Starting.transition(Finished).is_err(),
        "finished only after running"
    );
    assert!(Lost.transition(Running).is_err());
}

/// xorshift: deterministic, so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize]
    }
    fn flag(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

#[test]
fn random_walks_stay_inside_the_tables_and_guards() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..2000 {
        // outcome: never Verified without the verdict flag at that step, terminal is final
        let mut s = OutcomeState::Draft;
        for _ in 0..40 {
            let to = rng.pick(&OutcomeState::ALL);
            let g = OutcomeGuards {
                contract_approved: rng.flag(),
                plan_valid: rng.flag(),
                verdict_verified: rng.flag(),
                delivery_ready: rng.flag(),
            };
            match s.transition(to, &g) {
                Ok(n) => {
                    assert!(OUTCOME.contains(&(s, n)) && !s.is_terminal());
                    if n == OutcomeState::Verified {
                        assert!(g.verdict_verified);
                    }
                    if n == OutcomeState::Completed {
                        assert!(g.delivery_ready);
                    }
                    s = n;
                }
                Err(e) => assert_eq!((e.from, e.to), (s, to)),
            }
        }
        // work item: retries started never exceed the budget
        let max = (rng.next() % 4) as u32;
        let (mut w, mut used) = (WorkItemState::Created, 0u32);
        for _ in 0..60 {
            let to = rng.pick(&WorkItemState::ALL);
            let g = WorkItemGuards {
                retries_used: used,
                max_retries: max,
                independent_evidence: rng.flag(),
            };
            if let Ok(n) = w.transition(to, &g) {
                assert!(WORK_ITEM.contains(&(w, n)) && !w.is_terminal());
                if n == WorkItemState::Succeeded {
                    assert!(g.independent_evidence);
                }
                if n == WorkItemState::RetryableFailure {
                    assert!(used < max);
                }
                if w == WorkItemState::RetryableFailure && n == WorkItemState::Ready {
                    // the caller bumps `retries_used` when the retry starts
                    used += 1;
                    assert!(used <= max);
                }
                w = n;
            }
        }
        // decision: an approved decision was approved by a person on a current binding
        let mut d = DecisionState::Preparing;
        for _ in 0..30 {
            let to = rng.pick(&DecisionState::ALL);
            let g = DecisionGuards {
                actor: rng.pick(&[Actor::User, Actor::Orchestrator, Actor::Attempt]),
                binding_current: rng.flag(),
                expiry_reached: rng.flag(),
            };
            if let Ok(n) = d.transition(to, &g) {
                assert!(DECISION.contains(&(d, n)));
                if n == DecisionState::Approved {
                    assert!(g.actor == Actor::User && g.binding_current && !g.expiry_reached);
                }
                d = n;
            }
        }
    }
}

#[test]
fn wire_names_are_snake_case() {
    assert_eq!(
        serde_json::to_string(&OutcomeState::ContractPending).unwrap(),
        "\"contract_pending\""
    );
    assert_eq!(
        serde_json::to_string(&WorkItemState::WaitingDecision).unwrap(),
        "\"waiting_decision\""
    );
    assert_eq!(
        serde_json::from_str::<DecisionState>("\"needs_more_info\"").unwrap(),
        DecisionState::NeedsMoreInfo
    );
}
