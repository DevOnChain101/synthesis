//! The verdict (ADR-020 section 4): a pure function from the contract and the evidence facts to a
//! per-criterion verdict and the outcome verdict. No I/O, no clock. The store builds the facts
//! from rows that match their chained `evidence.recorded` event and calls this for `Verified`.

use std::collections::BTreeMap;

use super::contract::{Contract, TrustClass};

/// What one evidence row says, reduced to what the verdict needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceFact {
    pub trust: TrustClass,
    /// `passed`, `failed`, `error` or `skipped`.
    pub result: String,
    pub commit: String,
    pub check_id: String,
    pub criteria: Vec<String>,
    /// A flag disqualifies the row (`definitions_changed`, `tests_weakened`, `outside_envelope`).
    pub flagged: bool,
    /// The row is from the newest attempt of its work item (an older fence is superseded).
    pub fence_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CriterionVerdict {
    Passed,
    /// Evidence of the required class for the candidate commit failed (or errored: a runner that
    /// failed or timed out is never a pass).
    Failed,
    /// No acceptable evidence for the candidate commit; the reason is shown.
    Unverified(String),
    /// Acceptable evidence exists, but for another commit or a superseded attempt.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictReport {
    pub criteria: BTreeMap<String, CriterionVerdict>,
    /// Every mandatory criterion is `Passed`.
    pub verified: bool,
}

/// The class that can satisfy a criterion: `independent_check`, or `human_review` for a manual one.
pub fn required_class(manual: bool) -> TrustClass {
    if manual {
        TrustClass::HumanReview
    } else {
        TrustClass::IndependentCheck
    }
}

pub fn verdict(contract: &Contract, evidence: &[EvidenceFact], candidate: &str) -> VerdictReport {
    let mut criteria = BTreeMap::new();
    let mut verified = true;
    for c in &contract.criteria {
        let class = required_class(c.manual);
        // evidence for this criterion, from the check(s) the criterion names (all if it names none)
        let relevant: Vec<&EvidenceFact> = evidence
            .iter()
            .filter(|e| e.criteria.contains(&c.id))
            .filter(|e| c.check_ids.is_empty() || c.check_ids.contains(&e.check_id))
            .collect();
        let right_class: Vec<&&EvidenceFact> =
            relevant.iter().filter(|e| e.trust == class).collect();
        let fresh: Vec<&&&EvidenceFact> = right_class
            .iter()
            .filter(|e| e.commit == candidate && e.fence_current)
            .collect();
        let v = if fresh
            .iter()
            .any(|e| matches!(e.result.as_str(), "failed" | "error"))
        {
            // worst of: a failure of the same class for the same commit wins over any pass
            CriterionVerdict::Failed
        } else if fresh.iter().any(|e| e.result == "passed" && !e.flagged) {
            CriterionVerdict::Passed
        } else if fresh.iter().any(|e| e.result == "passed") {
            CriterionVerdict::Unverified("the evidence is flagged (definitions changed, tests weakened or files outside the envelope)".into())
        } else if fresh.iter().any(|e| e.result == "skipped") {
            CriterionVerdict::Unverified("the check was skipped".into())
        } else if right_class.iter().any(|e| e.result == "passed") {
            CriterionVerdict::Stale
        } else if relevant.is_empty() {
            CriterionVerdict::Unverified("no evidence".into())
        } else {
            CriterionVerdict::Unverified(format!(
                "only evidence below the required class ({class:?})"
            ))
        };
        if c.mandatory && v != CriterionVerdict::Passed {
            verified = false;
        }
        criteria.insert(c.id.clone(), v);
    }
    VerdictReport { criteria, verified }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::contract::fixtures::contract;

    fn fact(trust: TrustClass, result: &str, commit: &str, crit: &str) -> EvidenceFact {
        EvidenceFact {
            trust,
            result: result.into(),
            commit: commit.into(),
            check_id: "unit".into(),
            criteria: vec![crit.into()],
            flagged: false,
            fence_current: true,
        }
    }

    fn one(e: &[EvidenceFact]) -> CriterionVerdict {
        let r = verdict(&contract(&["c1"]), e, "C");
        r.criteria["c1"].clone()
    }

    use TrustClass::*;

    /// AC-08: the whole matrix of ADR-020 section 4 for one mandatory, non-manual criterion.
    #[test]
    fn only_current_independent_evidence_passes_a_mandatory_criterion() {
        let u = |m: &str| CriterionVerdict::Unverified(m.into());
        assert_eq!(one(&[]), u("no evidence"));
        for low in [
            ClaimedByWorker,
            ModelAssessment,
            ObservedInAttempt,
            HumanReview,
        ] {
            assert!(
                matches!(
                    one(&[fact(low, "passed", "C", "c1")]),
                    CriterionVerdict::Unverified(_)
                ),
                "{low:?} must not satisfy an independent criterion"
            );
        }
        assert_eq!(
            one(&[fact(IndependentCheck, "passed", "C", "c1")]),
            CriterionVerdict::Passed
        );
        assert_eq!(
            one(&[fact(IndependentCheck, "passed", "OLD", "c1")]),
            CriterionVerdict::Stale
        );
        // failed wins over passed of the same class and commit, in either order
        let (p, f) = (
            fact(IndependentCheck, "passed", "C", "c1"),
            fact(IndependentCheck, "failed", "C", "c1"),
        );
        assert_eq!(one(&[p.clone(), f.clone()]), CriterionVerdict::Failed);
        assert_eq!(one(&[f, p.clone()]), CriterionVerdict::Failed);
        // a runner that errored or timed out is a failure, never a pass
        assert_eq!(
            one(&[fact(IndependentCheck, "error", "C", "c1")]),
            CriterionVerdict::Failed
        );
        assert_eq!(
            one(&[fact(IndependentCheck, "skipped", "C", "c1")]),
            u("the check was skipped")
        );
        // flagged evidence is not a pass
        let mut flagged = p.clone();
        flagged.flagged = true;
        assert!(matches!(one(&[flagged]), CriterionVerdict::Unverified(_)));
        // a superseded attempt's evidence is stale
        let mut old = p.clone();
        old.fence_current = false;
        assert_eq!(one(&[old]), CriterionVerdict::Stale);
        // evidence for another criterion does not count
        assert!(matches!(
            one(&[fact(IndependentCheck, "passed", "C", "c2")]),
            CriterionVerdict::Unverified(_)
        ));
        // an old failure does not poison a current pass
        assert_eq!(
            one(&[fact(IndependentCheck, "failed", "OLD", "c1"), p]),
            CriterionVerdict::Passed
        );
    }

    #[test]
    fn a_criterion_that_names_checks_is_satisfied_only_by_them() {
        let mut c = contract(&["c1"]);
        c.criteria[0].check_ids = vec!["unit".into()];
        let mut other = fact(IndependentCheck, "passed", "C", "c1");
        other.check_id = "lint".into();
        assert!(!verdict(&c, std::slice::from_ref(&other), "C").verified);
        assert!(verdict(&c, &[fact(IndependentCheck, "passed", "C", "c1")], "C").verified);
    }

    #[test]
    fn a_manual_criterion_needs_a_human_review_and_only_mandatory_ones_decide() {
        let mut c = contract(&["c1", "c2"]);
        c.criteria[0].manual = true;
        c.criteria[0].required_trust = HumanReview;
        c.criteria[1].mandatory = false;
        let human = fact(HumanReview, "passed", "C", "c1");
        let r = verdict(&c, std::slice::from_ref(&human), "C");
        assert_eq!(r.criteria["c1"], CriterionVerdict::Passed);
        assert!(matches!(r.criteria["c2"], CriterionVerdict::Unverified(_)));
        assert!(r.verified, "an optional criterion does not block");
        // independent evidence does not review a manual criterion
        let ind = fact(IndependentCheck, "passed", "C", "c1");
        assert!(!verdict(&c, &[ind], "C").verified);
    }
}
