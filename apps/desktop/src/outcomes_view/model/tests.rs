use serde_json::json;
use synthesis_view::outcomes::{DecisionCard, InspectorTab};

use super::*;
use crate::live::ARM_MS;
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
    let p = s.ask.picker.as_ref().unwrap();
    assert_eq!(p.cursor(), None, "no preselection");
    // Enter with nothing chosen sends nothing and says what to do
    assert!(press(&mut s, "enter", 10_000).is_empty());
    assert!(
        s.ask
            .notice
            .as_deref()
            .unwrap()
            .contains("Nothing is chosen")
    );
    // step 1: the arrow moves the cursor (first Down: the first answer, in the brief's order); nothing is sent
    assert!(press(&mut s, "down", 10_000).is_empty());
    assert_eq!(
        s.ask.picker.as_ref().unwrap().choice(),
        Some("approve_with_w5")
    );
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
    assert!(s.ask.sending && press(&mut s, "enter", 10_000).is_empty());
}

#[test]
fn enter_waits_for_the_arming_delay_of_a_card_that_just_appeared() {
    let mut s = decisions();
    press(&mut s, "down", 0);
    assert!(press(&mut s, "enter", 399).is_empty(), "armed_until is 400");
    assert!(s.ask.notice.as_deref().unwrap().contains("just appeared"));
    assert!(!s.can_send(399) && s.can_send(400));
    assert_eq!(press(&mut s, "enter", 400).len(), 1);
}

#[test]
fn numbers_and_arrows_move_only_the_cursor_and_out_of_range_changes_nothing() {
    let mut s = decisions();
    press(&mut s, "4", 0);
    assert_eq!(
        s.ask.picker.as_ref().unwrap().choice(),
        Some("request_evidence")
    );
    press(&mut s, "9", 0);
    assert_eq!(
        s.ask.picker.as_ref().unwrap().choice(),
        Some("request_evidence")
    );
    press(&mut s, "up", 0);
    assert_eq!(s.ask.picker.as_ref().unwrap().choice(), Some("reject"));
    // from nothing, Up starts at the last answer, Down at the first: never a default
    let mut s = decisions();
    press(&mut s, "up", 0);
    assert_eq!(s.ask.picker.as_ref().unwrap().choice(), Some("edit_scope"));
    // Esc clears the choice (and re-arms); a second Esc leaves the column
    press(&mut s, "escape", 5);
    assert_eq!(s.ask.picker.as_ref().unwrap().cursor(), None);
    assert_eq!(s.ask.armed_until, 5 + ARM_MS);
    press(&mut s, "escape", 5);
    assert_eq!(s.pane, Pane::List);
}

#[test]
fn the_fixed_answers_follow_the_alternatives_in_order() {
    let s = decisions();
    let ids: Vec<_> = s
        .ask
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
    assert!(s.ask.picker.as_ref().unwrap().choice().is_some());
    let items = s.items.clone();
    let mut d = s.detail.clone().unwrap();
    // the same card and revision again (a refresh): the choice stands
    s.apply_snapshot(items.clone(), None, Some(d.clone()), None, 1000);
    assert_eq!(
        s.ask.picker.as_ref().unwrap().choice(),
        Some("approve_without_c3")
    );
    assert_eq!(
        s.ask.armed_until, ARM_MS,
        "not armed again for the same brief"
    );
    // a new revision (or a new brief hash) is another card: cursor on nothing, armed again, and it says so
    d.cards.as_mut().unwrap()[0].revision = 5;
    s.apply_snapshot(items.clone(), None, Some(d.clone()), None, 2000);
    assert_eq!(s.ask.picker.as_ref().unwrap().cursor(), None);
    assert_eq!(s.ask.armed_until, 2000 + ARM_MS);
    assert!(
        s.ask
            .notice
            .as_deref()
            .unwrap()
            .contains("changed since you saw it")
    );
    assert_eq!(s.active_card().unwrap().revision, 5);
    // the same for a new brief hash
    press(&mut s, "1", 3000);
    d.cards.as_mut().unwrap()[0].brief_sha256 = "b".repeat(64);
    s.apply_snapshot(items, None, Some(d), None, 4000);
    assert_eq!(s.ask.picker.as_ref().unwrap().cursor(), None);
}

#[test]
fn only_a_pending_decision_can_be_answered() {
    let mut s = decisions();
    // the third card of the sample is approved
    s.show_card(2, 0);
    assert_eq!(s.active_card().unwrap().state, "approved");
    assert!(s.ask.picker.is_none());
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
    assert!(s.ask.picker.is_none());
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
    assert!(s.detail.is_none() && s.ask.picker.is_none() && s.inspector.is_none());
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
    assert!(!s.ask.sending);
    // the refetch of the card the answer changed is not shown as "a card that changed"
    let (items, d) = (s.items.clone(), s.detail.clone());
    s.apply_snapshot(items, None, d, None, 20_000);
    assert_eq!(
        s.ask.notice.as_deref(),
        Some("Answer approve_with_w5: done")
    );
    assert_eq!(s.ask.picker.as_ref().unwrap().cursor(), None);
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
    assert!(s.ask.picker.is_none() && s.active_card().is_none());
    assert!(press(&mut s, "enter", 10_000).is_empty());
}

#[test]
fn a_link_that_dies_mid_send_does_not_leave_send_stuck_and_nothing_is_resent() {
    let mut s = decisions();
    press(&mut s, "1", 0);
    assert_eq!(press(&mut s, "enter", 10_000).len(), 1);
    assert!(s.ask.sending);
    // the link ended before the answer came back
    s.set_screen(Screen::Unreachable("closed".into()));
    assert!(!s.ask.sending);
    // r retries the connection and nothing else: no automatic resend of the old request
    assert_eq!(press(&mut s, "r", 10_000), vec![Effect::Retry]);
    // reconnected: a fresh snapshot, a new choice, and the card can be answered again
    let (items, d) = (s.items.clone(), s.detail.clone());
    s.apply_snapshot(items, None, d, None, 20_000);
    assert_eq!(s.screen, Screen::Ready);
    assert_eq!(s.ask.picker.as_ref().unwrap().cursor(), None);
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
    assert!(
        s.ask
            .notice
            .as_deref()
            .unwrap()
            .contains("cannot be revised")
    );
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
    assert_eq!(s.ask.notice.as_deref(), Some("Contract v1: done"));
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

// ------------------------------------------------------------------------ the guided enable flow

#[test]
fn off_to_disclosure_to_setting_needs_an_explicit_confirm_and_esc_cancels() {
    let mut s = State::new(Screen::Off);
    // Enter on the screen opens the disclosure; nothing is asked of the world
    assert!(press(&mut s, "enter", 0).is_empty());
    assert_eq!(
        s.guide,
        Some(Guide {
            step: Step::Setting,
            focus: None,
            busy: false,
            error: None,
            seq: 0
        })
    );
    // no default focus: Enter, other keys and a held Enter do nothing
    for key in ["enter", "r", "y", "space"] {
        assert!(press(&mut s, key, 0).is_empty(), "{key}");
    }
    assert_eq!(s.guide.as_ref().unwrap().focus, None);
    // Esc cancels, nothing was requested
    assert!(press(&mut s, "escape", 0).is_empty() && s.guide.is_none());
    // Tab, Tab, Enter: the one way to the effect, once
    press(&mut s, "enter", 0);
    press(&mut s, "tab", 0);
    assert_eq!(s.guide.as_ref().unwrap().focus, Some(Btn::Cancel));
    press(&mut s, "tab", 0);
    assert_eq!(s.guide.as_ref().unwrap().focus, Some(Btn::Confirm));
    assert_eq!(press(&mut s, "enter", 0), vec![Effect::EnableSetting]);
    assert!(s.guide.as_ref().unwrap().busy);
    assert!(press(&mut s, "enter", 0).is_empty(), "never twice");
    assert!(s.confirm_guide().is_empty());
    // a failure keeps the dialog open with the reason and re-arms nothing by itself
    s.guide_failed("synt.orchestrator.enabled: set by the managed settings file".into());
    let g = s.guide.clone().unwrap();
    assert!(!g.busy && g.error.is_some());
    // done: the consent is gone
    s.guide_done();
    assert!(s.guide.is_none());
}

#[test]
fn cancel_closes_without_an_effect_and_the_consent_is_not_kept() {
    let mut s = State::new(Screen::Off);
    s.open_guide();
    press(&mut s, "tab", 0);
    assert!(press(&mut s, "enter", 0).is_empty() && s.guide.is_none());
    // reopened: nothing is focused, whatever was before
    s.open_guide();
    assert_eq!(s.guide.as_ref().unwrap().focus, None);
    // a screen that is not the one the disclosure was opened for takes it away
    s.set_screen(Screen::Ready);
    assert!(s.guide.is_none());
}

#[test]
fn the_migration_is_its_own_step_with_its_own_confirm() {
    let mut s = State::new(Screen::NotReady("x".into()));
    // nothing migrates by itself, and the screen's Enter only opens the disclosure
    assert!(press(&mut s, "enter", 0).is_empty());
    assert_eq!(s.guide.as_ref().unwrap().step, Step::Migrate);
    press(&mut s, "tab", 0);
    press(&mut s, "tab", 0);
    assert_eq!(press(&mut s, "enter", 0), vec![Effect::Migrate(1)]);
    // the reply that migrated closes it; a refused one keeps it open
    s.guide_failed("enable reply without this build's integrity disclosure (id and text)".into());
    assert!(s.guide.as_ref().unwrap().error.is_some());
    // the setting step cannot be confirmed from the migration screen and the other way round
    let mut off = State::new(Screen::Off);
    off.open_guide();
    assert_eq!(off.confirm_guide(), vec![Effect::EnableSetting]);
}

#[test]
fn no_button_on_windows_ready_loading_or_unreachable() {
    for screen in [
        Screen::Windows,
        Screen::Ready,
        Screen::Loading,
        Screen::Unreachable("x".into()),
    ] {
        let mut s = State::new(screen);
        s.open_guide();
        assert!(s.guide.is_none());
        assert!(s.confirm_guide().is_empty());
    }
}

#[test]
fn esc_and_cancel_close_the_dialog_even_while_a_step_is_on_its_way() {
    for close in [0, 1] {
        let mut s = State::new(Screen::NotReady("x".into()));
        s.open_guide();
        assert_eq!(s.confirm_guide(), vec![Effect::Migrate(1)]);
        assert!(
            press(&mut s, "enter", 0).is_empty() && s.guide.is_some(),
            "other keys wait"
        );
        if close == 0 {
            press(&mut s, "escape", 0);
        } else {
            s.close_guide();
        }
        assert!(s.guide.is_none());
        // a late failure reply with no dialog open changes nothing
        s.guide_failed("late".into());
        assert!(s.guide.is_none());
    }
}

#[test]
fn a_late_migration_reply_is_said_on_the_screen_and_never_lands_in_a_newer_dialog() {
    let mut s = State::new(Screen::NotReady("x".into()));
    s.open_guide();
    assert_eq!(s.confirm_guide(), vec![Effect::Migrate(1)]);
    press(&mut s, "escape", 0);
    // the reply of confirm 1 arrives with no dialog open: a notice, the screen looks again
    s.enabled(1, Ok(()));
    assert!(s.notice.as_deref().unwrap().contains("completed"));
    assert_eq!(s.screen, Screen::Loading);
    // a failure of an older confirm does not land in a dialog opened since
    s.set_screen(Screen::NotReady("x".into()));
    s.open_guide();
    assert_eq!(s.notice, None, "a new dialog starts clean");
    assert_eq!(s.confirm_guide(), vec![Effect::Migrate(2)]);
    s.enabled(1, Err("old failure".into()));
    let g = s.guide.clone().unwrap();
    assert!(g.busy && g.error.is_none(), "{g:?}");
    assert!(s.notice.as_deref().unwrap().contains("old failure"));
    // the reply of the open one is its own
    s.enabled(2, Err("this one".into()));
    assert_eq!(s.guide.as_ref().unwrap().error.as_deref(), Some("this one"));
    s.enabled(2, Ok(()));
    assert!(s.guide.is_none() && s.screen == Screen::Loading);
}
