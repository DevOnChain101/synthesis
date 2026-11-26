use serde_json::json;
use synthesis_core::outcome::Capability;

use super::*;
use crate::superagent::outcomes::sample;

/// An absolute folder on every OS (`/tmp/x` on Unix, `C:\\...\\x` on Windows).
fn ws(name: &str) -> String {
    std::env::temp_dir().join(name).display().to_string()
}

const COMMIT: &str = "9433c678e4b1d2f0a7c35e8b61d9f4a02c7b5e13";

fn text() -> FormText {
    FormText {
        goal: "Move login to OIDC".into(),
        criteria: "Login works with the test IdP\n\nThe old API still answers 200\n".into(),
        checks: "cargo test -p auth\ncargo clippy -p auth -- -D warnings".into(),
        budget: String::new(),
        scope: "auth/**\ntests/**".into(),
        workspace: ws("app"),
        attempts: String::new(),
        turns: String::new(),
    }
}

fn prev() -> Contract {
    serde_json::from_value(sample::contract()["contract"].clone()).unwrap()
}

/// A new-outcome form whose base commit is known for `/work/app`.
fn new_form() -> Form {
    let mut f = Form::new_outcome(&ws("app"));
    f.resolving(&ws("app"));
    f.head_known(&ws("app"), Ok(COMMIT.into()));
    f
}

#[test]
fn the_text_becomes_a_contract_the_core_accepts_with_independent_checks_for_every_criterion() {
    let c = build(&text(), None).unwrap();
    assert_eq!(c.goal, "Move login to OIDC");
    // blank lines are not entries; ids are made, unique, and every criterion is backed by every check
    let ids: Vec<_> = c.criteria.iter().map(|k| k.id.as_str()).collect();
    assert_eq!(ids, ["c1", "c2"]);
    assert_eq!(c.checks.len(), 2);
    assert!(c.criteria.iter().all(|k| k.mandatory
        && !k.manual
        && k.required_trust == TrustClass::IndependentCheck
        && k.check_ids == ["t1", "t2"]));
    assert_eq!(c.envelope.allowed_paths, ["auth/**", "tests/**"]);
    assert_eq!(
        c.envelope.budgets.max_cost_usd, None,
        "a new goal names no money"
    );
    // reversible capabilities only: nothing irreversible is allowed by a form
    assert_eq!(
        c.envelope.mode(Capability::RepoWrite),
        CapabilityMode::Allow
    );
    assert_eq!(c.envelope.mode(Capability::GitPush), CapabilityMode::Deny);
    assert_eq!(validate_contract(&c), Ok(()));
}

#[test]
fn an_empty_goal_checks_scope_or_budget_is_refused_with_a_line_each() {
    let e = build(&FormText::default(), None).unwrap_err();
    for want in ["Goal", "Acceptance criteria", "Checks", "Write scope"] {
        assert!(e.iter().any(|l| l.starts_with(want)), "{want}: {e:?}");
    }
    let mut t = text();
    t.goal = "   \n ".into();
    t.checks = " \n".into();
    let e = build(&t, None).unwrap_err();
    assert_eq!(e.len(), 2, "{e:?}");
}

#[test]
fn the_cores_own_validation_speaks_for_what_the_form_does_not_check() {
    let mut t = text();
    t.scope = "../etc/passwd".into();
    let e = build(&t, None).unwrap_err();
    assert!(e[0].contains("path pattern"), "{e:?}");
    let mut t = text();
    t.budget = "-3".into();
    assert!(build(&t, None).unwrap_err()[0].starts_with("Budget"));
    t.budget = "inf".into();
    assert!(build(&t, None).is_err());
    let mut t = text();
    t.checks = "echo one\ntab\there".into();
    assert!(build(&t, None).unwrap_err()[0].contains("checks"));
}

#[test]
fn a_revision_matches_lines_by_exact_text_so_deleting_one_never_shifts_flags() {
    let p = prev();
    let t = text_of(&p);
    // unchanged text, unchanged contract
    assert_eq!(build(&t, Some(&p)).unwrap(), p);
    // deleting the middle criterion: c3 keeps its id and its manual, optional, human-review flags
    let mut t2 = t.clone();
    t2.criteria = t
        .criteria
        .lines()
        .filter(|l| !l.contains("old API"))
        .collect::<Vec<_>>()
        .join("\n");
    let c = build(&t2, Some(&p)).unwrap();
    assert_eq!(c.criteria.len(), 2);
    assert_eq!(
        c.criteria[1], p.criteria[2],
        "the manual criterion is untouched"
    );
    assert!(c.criteria[1].manual && !c.criteria[1].mandatory);
    // reordering keeps each line's own flags
    let mut t3 = t.clone();
    t3.criteria = t.criteria.lines().rev().collect::<Vec<_>>().join("\n");
    let c = build(&t3, Some(&p)).unwrap();
    assert_eq!(c.criteria[0], p.criteria[2]);
    assert_eq!(c.criteria[2], p.criteria[0]);
    // an edited line is a NEW criterion with the strict defaults, never the old one's flags
    let mut t4 = t.clone();
    t4.criteria = t4.criteria.replace(
        "Integration tests pass against OIDC",
        "Integration tests pass",
    );
    let c = build(&t4, Some(&p)).unwrap();
    let edited = &c.criteria[2];
    assert_eq!(edited.id, "c4");
    assert!(edited.mandatory && !edited.manual);
    assert_eq!(edited.required_trust, TrustClass::IndependentCheck);
    assert_eq!(edited.check_ids, ["t1", "t2"]);
    // checks too: the kept check keeps its id, a dropped one is no longer named, the budget is applied
    let mut t5 = t.clone();
    t5.checks = "cargo test -p auth".into();
    t5.budget = "6".into();
    let c = build(&t5, Some(&p)).unwrap();
    assert_eq!(c.checks.len(), 1);
    assert_eq!(c.checks[0].id, "t1");
    assert_eq!(c.criteria[1].check_ids, ["t1"]);
    assert_eq!(c.envelope.budgets.attempt_max_wall_secs, Some(1200));
    assert_eq!(c.envelope.budgets.max_cost_usd, Some(6.0));
    // exactly one check edited: it keeps its id, so the criteria keep naming it
    let mut t6 = t.clone();
    t6.checks = t6
        .checks
        .replace("cargo test -p auth", "cargo test -p auth --all");
    let c = build(&t6, Some(&p)).unwrap();
    assert_eq!(c.checks[0].id, "t1");
    assert_eq!(c.criteria[0].check_ids, ["t1"]);
    assert_eq!(c.criteria[1].check_ids, ["t1", "t2"]);
    // two edited at once are two new checks (nothing says which old one each replaces)
    let mut t7 = t6.clone();
    t7.checks = t7.checks.replace("cargo clippy", "cargo clippy --all");
    let c = build(&t7, Some(&p)).unwrap();
    assert_eq!(
        (c.checks[0].id.as_str(), c.checks[1].id.as_str()),
        ("t3", "t4")
    );
    assert!(c.criteria[1].check_ids.is_empty());
}

#[test]
fn a_criterion_whose_text_has_a_line_break_is_still_matched_by_the_flattened_line() {
    let mut p = prev();
    p.criteria[0].text = "Login through OIDC\nworks with the test IdP".into();
    p.criteria[0].check_ids = vec!["t1".into()];
    let c = build(&text_of(&p), Some(&p)).unwrap();
    // matched, so it is unchanged: id, flags and the original text (no spurious change in the diff)
    assert_eq!(c.criteria[0].id, "c1");
    assert_eq!(c.criteria[0].check_ids, ["t1"]);
    assert_eq!(c.criteria[0], p.criteria[0]);
}

#[test]
fn a_retry_after_a_lost_reply_carries_the_same_key_and_an_edit_a_new_one() {
    let mut f = new_form();
    let Some(Submit::Create {
        key: k1,
        base_commit,
        workspace,
        ..
    }) = f.submit(&text())
    else {
        panic!("{:?}", f.errors)
    };
    assert_eq!(
        (base_commit.as_str(), workspace.as_str()),
        (COMMIT, ws("app").as_str())
    );
    assert_eq!(f.stage, Stage::Sending);
    // nothing else is sent while one is in flight
    assert_eq!(f.submit(&text()), None);
    // the link failed: the answer is lost, the key is not spent, the text is the person's
    f.failed("the orchestrator closed the link".into(), false);
    assert_eq!((&f.stage, f.errors.len()), (&Stage::Edit, 1));
    let Some(Submit::Create { key: k2, .. }) = f.submit(&text()) else {
        panic!()
    };
    assert_eq!(k1, k2, "the retry cannot make a second draft");
    // an edit is a new request
    f.failed("the orchestrator closed the link".into(), false);
    let mut t = text();
    t.goal.push('!');
    let Some(Submit::Create { key: k3, .. }) = f.submit(&t) else {
        panic!()
    };
    assert_ne!(k1, k3);
    // the server answered (a refusal): the key is used up
    f.failed("refused".into(), true);
    let Some(Submit::Create { key: k4, .. }) = f.submit(&t) else {
        panic!()
    };
    assert_ne!(k3, k4);
    assert!(k1.starts_with("create-"));
}

#[test]
fn a_new_outcome_needs_an_absolute_folder_and_the_commit_of_that_folder() {
    let mut f = Form::new_outcome(&ws("app"));
    // not resolved yet
    assert_eq!(f.submit(&text()), None);
    assert!(f.errors[0].starts_with("Base commit"), "{:?}", f.errors);
    // resolved for another folder: not used
    f.resolving(&ws("other"));
    f.head_known(&ws("other"), Ok(COMMIT.into()));
    assert_eq!(f.submit(&text()), None);
    // the answer for a folder that is no longer asked about is dropped
    f.resolving(&ws("app"));
    f.head_known(&ws("other"), Err("late".into()));
    assert_eq!(f.head, Some(Head::Resolving(ws("app"))));
    f.head_known(&ws("app"), Err("no commit found".into()));
    assert_eq!(f.submit(&text()), None);
    assert!(f.errors.iter().any(|e| e.contains("no commit found")));
    // relative folder
    let mut t = text();
    t.workspace = "app".into();
    assert_eq!(f.submit(&t), None);
    assert!(f.errors.iter().any(|e| e.starts_with("Workspace")));
    // the field is looked at again only when the folder changed
    let f = new_form();
    assert_eq!(f.needs_head(&text()), None);
    t.workspace = ws("elsewhere");
    assert_eq!(f.needs_head(&t), Some(ws("elsewhere")));
}

#[test]
fn approving_is_a_separate_step_on_the_review_with_the_servers_version_and_hash() {
    let mut f = new_form();
    f.submit(&text()).unwrap();
    // nothing to approve before the server stored a contract
    assert_eq!(f.approve(), None);
    f.reviewing(sample::review(false));
    let (outcome, version, hash, key) = f.approve().unwrap();
    assert_eq!((outcome.as_str(), version), ("o_auth", 1));
    assert_eq!(hash, sample::review(false).hash);
    assert!(matches!(f.stage, Stage::Approving(_)));
    // a refusal brings the review back, with a new key (the old one is used up); a lost reply keeps it
    f.failed("lost".into(), false);
    let Stage::Review(r) = &f.stage else { panic!() };
    assert_eq!(r.key, key);
    f.approve().unwrap();
    f.failed("refused".into(), true);
    let Stage::Review(r) = &f.stage else { panic!() };
    assert_ne!(r.key, key);
}

#[test]
fn a_review_that_cannot_be_read_or_compared_cannot_be_approved() {
    let mut r = sample::review(true);
    assert!(r.approvable());
    // a version above the first with no diff at all is not approvable either
    let mut none = r.clone();
    none.diff = None;
    assert!(!none.approvable());
    r.diff = Some(Err("the diff is about another contract version".into()));
    assert!(!r.approvable());
    let mut f = new_form();
    f.reviewing(r);
    assert_eq!(f.approve(), None);
    let mut r = sample::review(false);
    r.contract = Err("unreadable".into());
    f.reviewing(r);
    assert_eq!(f.approve(), None);
}

#[test]
fn the_servers_diff_is_shown_as_sent_and_a_reply_for_another_version_is_refused() {
    let r = sample::review(true);
    let d = r.diff.unwrap().unwrap();
    assert_eq!(d.widens, ["allowed_paths: billing/**"]);
    assert_eq!(d.weakens, ["criterion c3 removed"]);
    assert_eq!(d.changes[1].new, "(none)");
    assert_eq!(d.base_version, "1");
    let v = json!({"version": 2, "hash": "h", "diff": {"changes": [], "widens": ["x"], "weakens": []}, "widens_scope": true});
    assert!(DiffView::from_json(&v, 3, "h").is_err());
    assert!(DiffView::from_json(&v, 2, "other").is_err());
    // the server's own flag must agree with its list
    let v = json!({"version": 2, "hash": "h", "diff": {"changes": [], "widens": ["x"], "weakens": []}, "widens_scope": false});
    assert!(DiffView::from_json(&v, 2, "h").is_err());
}

fn ro_text() -> FormText {
    FormText {
        goal: "Which modules still use the old logger?".into(),
        criteria: "Names every module\n\nGives a file and line for each\n".into(),
        workspace: ws("notes"),
        ..Default::default()
    }
}

#[test]
fn a_read_only_text_becomes_a_read_only_contract_the_core_accepts() {
    let c = build_read_only(&ro_text()).unwrap();
    assert_eq!(c.kind, synthesis_core::outcome::ContractKind::ReadOnly);
    // the implicit `report` criterion, then one manual criterion per rubric line; nothing writes or runs
    let ids: Vec<_> = c.criteria.iter().map(|k| k.id.as_str()).collect();
    assert_eq!(ids, ["report", "r1", "r2"]);
    assert!(c.criteria.iter().all(|k| k.manual && k.mandatory));
    assert!(c.checks.is_empty() && c.envelope.allowed_paths.is_empty());
    assert_eq!(c.envelope.budgets.max_cost_usd, None);
    assert_eq!(c.envelope.budgets.max_attempts_per_work_item, 3);
    // the rubric is optional
    let mut t = ro_text();
    t.criteria.clear();
    t.attempts = "5".into();
    let c = build_read_only(&t).unwrap();
    assert_eq!(c.criteria.len(), 1);
    assert_eq!(c.envelope.budgets.max_attempts_per_work_item, 5);
}

#[test]
fn a_read_only_goal_needs_a_goal_a_budget_and_attempts_in_range_with_a_line_each() {
    let errs = build_read_only(&FormText::default()).unwrap_err();
    assert!(errs.iter().any(|e| e.starts_with("Goal")));
    for bad in ["0", "11", "two", "-1"] {
        let mut t = ro_text();
        t.attempts = bad.into();
        let errs = build_read_only(&t).unwrap_err();
        assert!(errs[0].starts_with("Attempts"), "{bad}: {errs:?}");
    }
    // a legacy money cap is optional; when typed it is a positive number
    let mut t = ro_text();
    t.budget = "0".into();
    assert!(build_read_only(&t).is_err());
    t.budget = "2".into();
    assert_eq!(
        build_read_only(&t).unwrap().envelope.budgets.max_cost_usd,
        Some(2.0)
    );
}

#[test]
fn a_read_only_form_needs_no_commit_and_sends_an_empty_base() {
    let mut f = Form::new_read_only(&ws("notes"));
    // no HEAD is ever asked for
    assert_eq!(f.needs_head(&ro_text()), None);
    // an absolute folder is the only folder rule
    let mut t = ro_text();
    t.workspace = "notes".into();
    assert_eq!(f.submit(&t), None);
    assert!(
        f.errors.iter().any(|e| e.starts_with("Folder")),
        "{:?}",
        f.errors
    );
    let Some(Submit::Create {
        workspace,
        base_commit,
        contract,
        key,
    }) = f.submit(&ro_text())
    else {
        panic!("{:?}", f.errors)
    };
    assert_eq!((workspace, base_commit), (ws("notes"), String::new()));
    assert_eq!(
        contract.kind,
        synthesis_core::outcome::ContractKind::ReadOnly
    );
    assert!(key.starts_with("create-"));
    // sent again unchanged after a lost reply: the same key
    f.failed("lost".into(), false);
    let Some(Submit::Create { key: again, .. }) = f.submit(&ro_text()) else {
        panic!()
    };
    assert_eq!(again, key);
    // approval is the same separate act, on the server's version and hash
    assert_eq!(f.approve(), None);
    f.reviewing(sample::review(false));
    assert!(f.approve().is_some());
}

#[test]
fn turns_per_attempt_is_empty_for_none_or_a_whole_number_in_range_and_a_revision_starts_from_the_stored_one()
 {
    // empty: the contract names none (the old bytes), for a write and a read-only goal
    assert_eq!(
        build(&text(), None).unwrap().envelope.budgets.max_turns,
        None
    );
    let mut t = text();
    t.turns = " 30 ".into();
    assert_eq!(
        build(&t, None).unwrap().envelope.budgets.max_turns,
        Some(30)
    );
    let mut r = FormText {
        goal: "g".into(),
        budget: "2".into(),
        ..Default::default()
    };
    r.turns = "200".into();
    assert_eq!(
        build_read_only(&r).unwrap().envelope.budgets.max_turns,
        Some(200)
    );
    for bad in ["0", "201", "-1", "1.5", "many"] {
        t.turns = bad.into();
        r.turns = bad.into();
        for e in [
            build(&t, None).unwrap_err(),
            build_read_only(&r).unwrap_err(),
        ] {
            assert!(
                e.iter()
                    .any(|l| l.starts_with("Turns: a whole number from 1 to 200")),
                "{bad}: {e:?}"
            );
        }
    }
    // a revision shows the stored number and, emptied, removes it (the server's diff calls that widening)
    let mut p = prev();
    p.envelope.budgets.max_turns = Some(20);
    let mut t = text_of(&p);
    assert_eq!(t.turns, "20");
    t.workspace = ws("app");
    assert_eq!(
        build(&t, Some(&p)).unwrap().envelope.budgets.max_turns,
        Some(20)
    );
    t.turns.clear();
    assert_eq!(
        build(&t, Some(&p)).unwrap().envelope.budgets.max_turns,
        None
    );
}
