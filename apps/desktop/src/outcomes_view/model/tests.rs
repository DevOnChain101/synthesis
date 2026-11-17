use serde_json::json;
use synthesis_view::outcomes::{DecisionCard, InspectorTab};

use super::*;
use crate::outcomes_view::{form, sample};

fn k(name: &str) -> Key<'_> {
    Key {
        name,
        shift: false,
        other_mod: false,
    }
}

/// The sample, with the keys in the Decisions tab of the centre column.
fn decisions() -> State {
    let mut s = sample::state();
    s.pane = Pane::Centre;
    s.centre = CentreTab::Decisions;
    s
}

fn press(s: &mut State, key: &str, now: i64) -> Vec<Effect> {
    s.key(&k(key), now).unwrap_or_default()
}

#[test]
fn nothing_is_chosen_for_the_person_and_a_choice_takes_two_steps() {
    let mut s = decisions();
    let p = s.picker.as_ref().unwrap();
    assert_eq!(p.cursor(), None, "no preselection");
    // Enter with nothing chosen sends nothing and says what to do
    assert!(press(&mut s, "enter", 10_000).is_empty());
    assert!(s.notice.as_deref().unwrap().contains("Nothing is chosen"));
    // step 1: the arrow moves the cursor (first Down: the first answer, in the brief's order); nothing is sent
    assert!(press(&mut s, "down", 10_000).is_empty());
    assert_eq!(s.picker.as_ref().unwrap().choice(), Some("approve_with_w5"));
    // step 2: Enter sends the card's own revision and hash
    let e = press(&mut s, "enter", 10_000);
    let [
        Effect::Resolve {
            outcome,
            card,
            choice,
        },
    ] = e.as_slice()
    else {
        panic!("{e:?}")
    };
    assert_eq!(
        (outcome.as_str(), card.id.as_str(), choice.as_str()),
        ("o_auth", "d_7f3a", "approve_with_w5")
    );
    assert_eq!(card.revision, 4);
    assert_eq!(card.brief_sha256.len(), 64);
    // one answer at a time
    assert!(s.sending && press(&mut s, "enter", 10_000).is_empty());
}

#[test]
fn enter_waits_for_the_arming_delay_of_a_card_that_just_appeared() {
    let mut s = decisions();
    press(&mut s, "down", 0);
    assert!(press(&mut s, "enter", 399).is_empty(), "armed_until is 400");
    assert!(s.notice.as_deref().unwrap().contains("just appeared"));
    assert!(!s.can_send(399) && s.can_send(400));
    assert_eq!(press(&mut s, "enter", 400).len(), 1);
}

#[test]
fn numbers_and_arrows_move_only_the_cursor_and_out_of_range_changes_nothing() {
    let mut s = decisions();
    press(&mut s, "4", 0);
    assert_eq!(
        s.picker.as_ref().unwrap().choice(),
        Some("request_evidence")
    );
    press(&mut s, "9", 0);
    assert_eq!(
        s.picker.as_ref().unwrap().choice(),
        Some("request_evidence")
    );
    press(&mut s, "up", 0);
    assert_eq!(s.picker.as_ref().unwrap().choice(), Some("reject"));
    // from nothing, Up starts at the last answer, Down at the first: never a default
    let mut s = decisions();
    press(&mut s, "up", 0);
    assert_eq!(s.picker.as_ref().unwrap().choice(), Some("edit_scope"));
    // Esc clears the choice (and re-arms); a second Esc leaves the column
    press(&mut s, "escape", 5);
    assert_eq!(s.picker.as_ref().unwrap().cursor(), None);
    assert_eq!(s.armed_until, 5 + ARM_MS);
    press(&mut s, "escape", 5);
    assert_eq!(s.pane, Pane::List);
}

#[test]
fn the_fixed_answers_follow_the_alternatives_in_order() {
    let s = decisions();
    let ids: Vec<_> = s
        .picker
        .as_ref()
        .unwrap()
        .options
        .iter()
        .map(|o| o.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "approve_with_w5",
            "approve_without_c3",
            "reject",
            "request_evidence",
            "edit_scope"
        ]
    );
}

#[test]
fn a_card_that_changed_is_shown_again_with_nothing_chosen() {
    let mut s = decisions();
    press(&mut s, "2", 0);
    assert!(s.picker.as_ref().unwrap().choice().is_some());
    let items = s.items.clone();
    let mut d = s.detail.clone().unwrap();
    // the same card and revision again (a refresh): the choice stands
    s.apply_snapshot(items.clone(), None, Some(d.clone()), None, 1000);
    assert_eq!(
        s.picker.as_ref().unwrap().choice(),
        Some("approve_without_c3")
    );
    assert_eq!(s.armed_until, ARM_MS, "not armed again for the same brief");
    // a new revision (or a new brief hash) is another card: cursor on nothing, armed again, and it says so
    d.cards.as_mut().unwrap()[0].revision = 5;
    s.apply_snapshot(items.clone(), None, Some(d.clone()), None, 2000);
    assert_eq!(s.picker.as_ref().unwrap().cursor(), None);
    assert_eq!(s.armed_until, 2000 + ARM_MS);
    assert!(
        s.notice
            .as_deref()
            .unwrap()
            .contains("changed since you saw it")
    );
    assert_eq!(s.active_card().unwrap().revision, 5);
    // the same for a new brief hash
    press(&mut s, "1", 3000);
    d.cards.as_mut().unwrap()[0].brief_sha256 = "b".repeat(64);
    s.apply_snapshot(items, None, Some(d), None, 4000);
    assert_eq!(s.picker.as_ref().unwrap().cursor(), None);
}

#[test]
fn only_a_pending_decision_can_be_answered() {
    let mut s = decisions();
    // the third card of the sample is approved
    s.show_card(2, 0);
    assert_eq!(s.active_card().unwrap().state, "approved");
    assert!(s.picker.is_none());
    for key in ["down", "1", "enter"] {
        assert!(press(&mut s, key, 10_000).is_empty(), "{key}");
    }
    assert!(!s.can_send(10_000));
    // a card that stops being pending takes the picker away
    s.show_card(0, 0);
    press(&mut s, "1", 0);
    let mut d = s.detail.clone().unwrap();
    d.cards.as_mut().unwrap()[0].state = "expired".into();
    s.apply_snapshot(s.items.clone(), None, Some(d), None, 9);
    assert!(s.picker.is_none());
}

#[test]
fn a_detail_for_another_outcome_or_tab_is_late_and_dropped() {
    let mut s = sample::state();
    let items = s.items.clone();
    let other = sample::detail(&items[1]);
    let before = s.detail.clone();
    s.apply_snapshot(items.clone(), None, Some(other), None, 0);
    assert_eq!(s.detail, before, "a detail of o_ci under o_auth");
    // the inspector content of another tab than the open one
    s.itab = InspectorTab::Workers;
    let ov = sample::inspector(InspectorTab::Overview, &items[0]);
    s.apply_snapshot(items.clone(), None, before.clone(), Some(ov.clone()), 0);
    assert_eq!(s.inspector, None);
    s.apply_inspector("o_ci", sample::inspector(InspectorTab::Workers, &items[0]));
    assert_eq!(s.inspector, None, "another outcome");
    s.apply_inspector(
        "o_auth",
        sample::inspector(InspectorTab::Workers, &items[0]),
    );
    assert!(s.inspector.is_some());
}

#[test]
fn selecting_an_outcome_drops_the_old_detail_and_asks_for_the_new_one() {
    let mut s = sample::state();
    assert_eq!(s.pane, Pane::List);
    assert_eq!(
        press(&mut s, "down", 0),
        vec![Effect::Select("o_ci".into())]
    );
    assert!(s.detail.is_none() && s.picker.is_none() && s.inspector.is_none());
    // already selected, or past the end: nothing
    assert!(s.select(1).is_empty() && s.select(99).is_empty());
    assert_eq!(
        press(&mut s, "up", 0),
        vec![Effect::Select("o_auth".into())]
    );
}

#[test]
fn tab_moves_between_the_columns_and_the_inspector_tabs_follow_arrows_and_numbers() {
    let mut s = sample::state();
    press(&mut s, "tab", 0);
    assert_eq!(s.pane, Pane::Centre);
    press(&mut s, "tab", 0);
    assert_eq!(s.pane, Pane::Inspector);
    press(&mut s, "tab", 0);
    assert_eq!(s.pane, Pane::List);
    s.key(
        &Key {
            name: "tab",
            shift: true,
            other_mod: false,
        },
        0,
    );
    assert_eq!(s.pane, Pane::Inspector);
    assert_eq!(
        press(&mut s, "right", 0),
        vec![Effect::Inspect(InspectorTab::Workers)]
    );
    assert_eq!(
        press(&mut s, "4", 0),
        vec![Effect::Inspect(InspectorTab::Diff)]
    );
    assert!(press(&mut s, "4", 0).is_empty(), "already open");
    assert_eq!(
        press(&mut s, "left", 0),
        vec![Effect::Inspect(InspectorTab::Activity)]
    );
    assert_eq!(press(&mut s, "down", 0), vec![Effect::Scroll(1)]);
    // the centre tabs
    s.pane = Pane::Centre;
    press(&mut s, "left", 0);
    assert_eq!(s.centre, CentreTab::Goal);
    press(&mut s, "right", 0);
    press(&mut s, "right", 0);
    press(&mut s, "right", 0);
    assert_eq!(s.centre, CentreTab::History);
}

#[test]
fn a_key_with_a_command_modifier_is_not_ours_and_the_other_screens_only_retry() {
    let mut s = sample::state();
    assert!(
        s.key(
            &Key {
                name: "1",
                shift: false,
                other_mod: true
            },
            0
        )
        .is_none(),
        "Cmd+1 switches the view"
    );
    assert!(s.key(&k("f12"), 0).is_none() || s.pane == Pane::List);
    for screen in [
        Screen::Off,
        Screen::Unreachable("x".into()),
        Screen::NotReady("y".into()),
    ] {
        let mut s = State::new(screen);
        assert_eq!(press(&mut s, "r", 0), vec![Effect::Retry]);
        assert!(press(&mut s, "down", 0).is_empty());
    }
    for screen in [Screen::Windows, Screen::Loading] {
        assert!(State::new(screen).key(&k("r"), 0).is_none());
    }
}

#[test]
fn an_answer_ends_the_send_and_the_next_one_is_a_new_choice() {
    let mut s = decisions();
    press(&mut s, "1", 0);
    assert_eq!(press(&mut s, "enter", 10_000).len(), 1);
    s.answered("Answer approve_with_w5: done".into());
    assert!(!s.sending);
    // the refetch of the card the answer changed is not shown as "a card that changed"
    let (items, d) = (s.items.clone(), s.detail.clone());
    s.apply_snapshot(items, None, d, None, 20_000);
    assert_eq!(s.notice.as_deref(), Some("Answer approve_with_w5: done"));
    assert_eq!(s.picker.as_ref().unwrap().cursor(), None);
}

#[test]
fn a_brief_that_names_a_preferred_option_is_never_made_answerable() {
    let c = json!({"id": "d1", "kind": "k", "state": "pending", "revision": 1, "brief_sha256": "a".repeat(64),
        "brief": {"alternatives": [{"id": "A", "recommended": true}]}});
    assert!(DecisionCard::from_json(&c).is_err());
    // the models refuse the whole list: the tab shows the error, there is no card and no picker
    let mut s = decisions();
    let mut d = s.detail.clone().unwrap();
    d.cards = Err("the decision brief carries a \"recommended\" field".into());
    s.apply_snapshot(s.items.clone(), None, Some(d), None, 0);
    assert!(s.picker.is_none() && s.active_card().is_none());
    assert!(press(&mut s, "enter", 10_000).is_empty());
}

#[test]
fn a_link_that_dies_mid_send_does_not_leave_send_stuck_and_nothing_is_resent() {
    let mut s = decisions();
    press(&mut s, "1", 0);
    assert_eq!(press(&mut s, "enter", 10_000).len(), 1);
    assert!(s.sending);
    // the link ended before the answer came back
    s.set_screen(Screen::Unreachable("closed".into()));
    assert!(!s.sending);
    // r retries the connection and nothing else: no automatic resend of the old request
    assert_eq!(press(&mut s, "r", 10_000), vec![Effect::Retry]);
    // reconnected: a fresh snapshot, a new choice, and the card can be answered again
    let (items, d) = (s.items.clone(), s.detail.clone());
    s.apply_snapshot(items, None, d, None, 20_000);
    assert_eq!(s.screen, Screen::Ready);
    assert_eq!(s.picker.as_ref().unwrap().cursor(), None);
    assert!(press(&mut s, "enter", 30_000).is_empty());
    press(&mut s, "2", 30_000);
    assert_eq!(press(&mut s, "enter", 30_000).len(), 1);
}

#[test]
fn new_outcome_opens_the_form_on_the_workspace_and_reads_its_commit_and_the_form_owns_the_keys() {
    let mut s = sample::state();
    let e = press(&mut s, "n", 0);
    assert_eq!(e, vec![Effect::Head(sample::workspace())]);
    let f = s.form.as_ref().unwrap();
    assert!(f.is_new() && f.initial.workspace == sample::workspace());
    // with the form open, none of the tab's keys act behind it
    assert_eq!(s.key(&k("r"), 0), None);
    assert_eq!(s.key(&k("down"), 0), None);
    s.close_form();
    assert!(s.form.is_none());
    assert!(s.key(&k("down"), 0).is_some());
}

#[test]
fn revise_starts_from_the_current_contract_and_the_revision_that_is_shown() {
    let mut s = sample::state();
    assert!(press(&mut s, "v", 0).is_empty());
    let f = s.form.as_ref().unwrap();
    assert_eq!(
        f.mode,
        form::Mode::Revise {
            outcome: "o_auth".into(),
            revision: 12
        }
    );
    assert!(f.initial.goal.starts_with("Move login to OIDC"));
    assert_eq!(f.initial.scope, "auth/**\ntests/**");
    // a contract this build cannot read in full is not revised from a guess
    let mut s = sample::state();
    s.detail.as_mut().unwrap().current = None;
    press(&mut s, "v", 0);
    assert!(s.form.is_none());
    assert!(s.notice.as_deref().unwrap().contains("cannot be revised"));
}

#[test]
fn review_and_approve_is_offered_only_for_a_contract_that_waits() {
    let mut s = sample::state();
    assert!(press(&mut s, "a", 0).is_empty());
    assert!(
        s.form.is_none(),
        "an approved contract has nothing to approve"
    );
    if let Some(Ok(c)) = s.detail.as_mut().map(|d| &mut d.contract) {
        c.approved = Some(false);
    }
    assert_eq!(press(&mut s, "a", 0), vec![Effect::Review("o_auth".into())]);
    assert!(matches!(
        s.form.as_ref().unwrap().mode,
        form::Mode::Draft { .. }
    ));
}

#[test]
fn what_the_worker_says_reaches_an_open_form_and_an_approval_closes_it() {
    let mut s = sample::state();
    press(&mut s, "n", 0);
    s.apply_form(FormMsg::Head {
        ws: sample::workspace(),
        result: Ok("a".repeat(40)),
    });
    s.apply_form(FormMsg::Review(Box::new(sample::review(false))));
    assert!(s.form_review().is_some());
    // approving asks for exactly the reviewed version and hash, once
    let e = s.approve();
    assert_eq!(e.len(), 1);
    assert!(matches!(&e[0], Effect::Approve { version: 1, .. }));
    assert!(
        s.approve().is_empty(),
        "no second approval while one is on its way"
    );
    s.apply_form(FormMsg::Approved("Contract v1: done".into()));
    assert!(s.form.is_none());
    assert_eq!(s.notice.as_deref(), Some("Contract v1: done"));
    // a reply for a form that was closed is dropped
    s.apply_form(FormMsg::Failed {
        text: "late".into(),
        spent: true,
    });
    assert!(s.form.is_none());
}

#[test]
fn a_draft_whose_read_failed_can_be_asked_again_or_closed() {
    let mut s = sample::state();
    if let Some(Ok(c)) = s.detail.as_mut().map(|d| &mut d.contract) {
        c.approved = Some(false);
    }
    press(&mut s, "a", 0);
    s.apply_form(FormMsg::Failed {
        text: "unreachable".into(),
        spent: false,
    });
    assert_eq!(s.form.as_ref().unwrap().errors, ["unreachable"]);
    assert_eq!(
        s.submit(&FormText::default()),
        vec![Effect::Review("o_auth".into())]
    );
    assert_eq!(s.form.as_ref().unwrap().stage, form::Stage::Sending);
    s.close_form();
    assert!(s.form.is_none());
}
