//! The docked decision card of the Super Agent view (moved here from the TUI view's popup in S0): geometry, the key
//! model and every safety rule of the old popup, on the sample data. The model under test: the card opens by itself
//! and never takes the keys; Alt+A (or a click on it) gives them to it; Esc or Alt+A gives them back; a second Esc
//! soon after, or "Later", dismisses it. Choosing and sending are two steps, never preselected.

use std::{cell::RefCell, rc::Rc};

use gpui::{Bounds, Entity, Pixels, TestAppContext, VisualTestContext, px, size};

use super::OutcomesView;
use super::model::{CentreTab, Pane, Screen};
use super::popup::{OpenSession, Shown};
use crate::live::ARM_MS;
use crate::theme;

const BIG: (f32, f32) = (1400., 900.);
const SMALL: (f32, f32) = (1100., 700.);

/// The sample view on its `centre` tab (the card is drawn on every tab but Decisions), the clock fixed.
fn open(
    cx: &mut TestAppContext,
    (w, h): (f32, f32),
    centre: CentreTab,
) -> (Entity<OutcomesView>, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::apply("JetBrains Mono", cx);
    });
    let (o, v) = cx.add_window_view(|_, cx| {
        let mut o = OutcomesView::sample(cx);
        o.sample_clock();
        o.state.centre = centre;
        o
    });
    v.simulate_resize(size(px(w), px(h)));
    o.update_in(v, |o, window, cx| window.focus(&o.focus, cx));
    v.run_until_parked();
    // the arming delay of the sample's own cards is over
    tick(&o, v, 10_000);
    (o, v)
}

fn open_goal(cx: &mut TestAppContext) -> (Entity<OutcomesView>, &mut VisualTestContext) {
    open(cx, BIG, CentreTab::Goal)
}

fn b(v: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
    v.debug_bounds(sel)
        .unwrap_or_else(|| panic!("no element {sel}"))
}

fn press(o: &Entity<OutcomesView>, v: &mut VisualTestContext, k: &'static str) -> bool {
    let ev = gpui::KeyDownEvent {
        keystroke: gpui::Keystroke::parse(k).unwrap(),
        is_held: false,
        prefer_character_input: false,
    };
    let r = o.update_in(v, |o, window, cx| o.on_key(&ev, window, cx));
    v.run_until_parked();
    r
}

fn has_keys(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> bool {
    o.update_in(v, |o, window, _| o.popup_has_keys(window))
}

fn cursor(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> Option<Option<usize>> {
    o.read_with(v, |o, _| match o.pop.shown.as_ref() {
        Some(Shown::Card(p, _)) => p.ask.picker.as_ref().map(|p| p.cursor()),
        _ => None,
    })
}

fn shown(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> bool {
    o.read_with(v, |o, _| o.pop.shown.is_some())
}

fn answers(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> u64 {
    o.read_with(v, |o, _| o.state.answers)
}

fn notice(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> Option<String> {
    o.read_with(v, |o, _| match o.pop.shown.as_ref() {
        Some(Shown::Card(p, _)) => p.ask.notice.clone(),
        _ => None,
    })
}

/// The clock of the view moves on by `ms`.
fn tick(o: &Entity<OutcomesView>, v: &mut VisualTestContext, ms: i64) {
    o.update(v, |o, _| o.pop.fixed_now = o.pop.fixed_now.map(|n| n + ms));
}

/// The pending card of the sample changes (a new revision, another id, fewer sections, ...).
fn edit_card(
    o: &Entity<OutcomesView>,
    v: &mut VisualTestContext,
    f: impl FnOnce(&mut synthesis_view::outcomes::DecisionCard),
) {
    o.update(v, |o, cx| {
        if let Some(d) = o.state.detail.as_mut()
            && let Ok(cards) = d.cards.as_mut()
        {
            f(&mut cards[0]);
        }
        cx.notify();
    });
    v.run_until_parked();
}

#[gpui::test]
fn nothing_is_counted_or_answered_while_the_link_is_not_ready(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert!(
        o.read_with(v, |o, _| o.waiting_count()) > 0,
        "the sample has a decision"
    );
    o.update(v, |o, cx| {
        o.state.screen = Screen::Off;
        cx.notify();
    });
    v.run_until_parked();
    assert_eq!(
        o.read_with(v, |o, _| o.waiting_count()),
        0,
        "off: the badge is 0"
    );
    assert!(v.debug_bounds("decision").is_none());
    assert!(!press(&o, v, "alt-a"), "off: Alt+A is not ours");
    o.update(v, |o, cx| {
        o.state.screen = Screen::Windows;
        cx.notify();
    });
    assert_eq!(o.read_with(v, |o, _| o.waiting_count()), 0, "windows too");
}

#[gpui::test]
fn the_count_is_decisions_and_waiting_sessions_oldest_first(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert_eq!(o.read_with(v, |o, _| o.waiting_count()), 1);
    o.update(v, |o, cx| {
        o.set_waiting_tools(vec![("s1".into(), "refactor-auth: Bash".into())], cx)
    });
    v.run_until_parked();
    let kinds: Vec<_> = o.read_with(v, |o, _| {
        o.approval_rows()
            .into_iter()
            .map(|r| r.kind.label())
            .collect()
    });
    // the decision has waited 9 minutes, the session was first seen just now
    assert_eq!(kinds, ["Decision", "Tool"]);
    assert_eq!(o.read_with(v, |o, _| o.waiting_count()), 2);
    // a session that stops waiting is no longer counted
    o.update(v, |o, cx| o.set_waiting_tools(vec![], cx));
    assert_eq!(o.read_with(v, |o, _| o.waiting_count()), 1);
}

#[gpui::test]
fn the_card_opens_by_itself_under_the_tabs_without_the_keys_and_the_decisions_tab_has_none(
    cx: &mut TestAppContext,
) {
    for size in [BIG, SMALL] {
        let (o, v) = open(cx, size, CentreTab::Goal);
        let (d, centre) = (b(v, "decision"), b(v, "out-centre"));
        assert!(d.origin.y >= b(v, "out-centre-body").origin.y);
        assert!(d.bottom() <= centre.bottom() && d.right() <= centre.right());
        for sel in [
            "decision-opt-0",
            "decision-opt-1",
            "decision-opt-2",
            "decision-opt-3",
            "decision-opt-4",
        ] {
            assert!(
                b(v, sel).bottom() <= d.bottom(),
                "the answers stay in the card: {sel}"
            );
        }
        assert!(
            v.debug_bounds("decision-send").is_some() && v.debug_bounds("decision-later").is_some()
        );
        assert!(!has_keys(&o, v), "it never takes the keys by itself");
        assert_eq!(cursor(&o, v), Some(None), "no preselection");
        // the Decisions tab draws the same card in full: no second one
        o.update(v, |o, cx| {
            o.state.centre = CentreTab::Decisions;
            cx.notify();
        });
        v.run_until_parked();
        assert!(v.debug_bounds("decision").is_none() && v.debug_bounds("out-card").is_some());
    }
}

#[gpui::test]
fn an_auto_opened_card_never_reads_the_views_keys(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert!(shown(&o, v) && !has_keys(&o, v));
    for k in [
        "1",
        "2",
        "9",
        "enter",
        "tab",
        "escape",
        "shift-enter",
        "shift-2",
        "shift-tab",
    ] {
        press(&o, v, k);
        assert_eq!(cursor(&o, v), Some(None), "{k} chose nothing");
        assert_eq!(answers(&o, v), 0, "{k} sent nothing");
        assert!(shown(&o, v), "{k} did not close it");
        assert!(!has_keys(&o, v), "{k} did not focus it");
        tick(&o, v, 1000);
    }
}

#[gpui::test]
fn alt_a_gives_the_card_the_keys_choose_then_enter_sends_and_esc_gives_them_back(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a"));
    assert!(has_keys(&o, v));
    // the whole brief has to be shown once before an answer can be sent (the sample hides sections when collapsed)
    assert!(press(&o, v, "tab"));
    assert!(press(&o, v, "down"));
    assert_eq!(cursor(&o, v), Some(Some(0)));
    // Enter right after the move is inert (ARM_MS), whatever was chosen
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 0, "inside the arming window after the move");
    // step 1: a number chooses (and sends nothing), step 2: Enter after the window sends
    assert!(press(&o, v, "2"));
    assert_eq!(cursor(&o, v), Some(Some(1)));
    assert_eq!(answers(&o, v), 0);
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 1, "sent after a choice and the window");
    // Tab again collapses it, and having seen it all once, Send stays allowed for this card
    assert!(press(&o, v, "tab"));
    assert!(o.read_with(v, |o, _| matches!(
        o.pop.shown.as_ref(),
        Some(Shown::Card(p, _)) if !p.full && p.seen_all && !p.send_blocked()
    )));
    // Esc gives the keys back and leaves the card
    assert!(press(&o, v, "escape"));
    assert!(!has_keys(&o, v) && shown(&o, v));
    // Alt+A again takes them, and a third time gives them back without closing
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    assert!(press(&o, v, "alt-a") && !has_keys(&o, v) && shown(&o, v));
}

#[gpui::test]
fn keys_reach_the_focused_card_through_the_windows_own_dispatch(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    o.update_in(v, |o, window, cx| o.focus_decision(window, cx));
    v.run_until_parked();
    assert!(has_keys(&o, v));
    // the real dispatch: the card holds the focus and the key bubbles to the view's handler
    v.simulate_keystrokes("2");
    assert_eq!(cursor(&o, v), Some(Some(1)));
    v.simulate_keystrokes("escape");
    assert!(!has_keys(&o, v) && shown(&o, v));
    // and now the view has the keys again: a digit chooses nothing
    v.simulate_keystrokes("3");
    assert_eq!(cursor(&o, v), Some(Some(1)));
}

#[gpui::test]
fn a_second_esc_or_later_dismisses_it_until_a_new_card_or_alt_a(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a"));
    assert!(press(&o, v, "escape"));
    assert!(shown(&o, v));
    // an immediate second Esc is the first one auto-repeating: swallowed, nothing dismissed
    assert!(press(&o, v, "escape"));
    assert!(shown(&o, v), "a held Esc does not dismiss");
    tick(&o, v, 200);
    assert!(press(&o, v, "escape"));
    assert!(!shown(&o, v) && !has_keys(&o, v));
    assert!(v.debug_bounds("decision").is_none());
    // it stays closed for the same card (a refresh does not bring it back)...
    o.update(v, |_, cx| cx.notify());
    v.run_until_parked();
    assert!(!shown(&o, v));
    // ...and Alt+A brings it back, with the keys
    assert!(press(&o, v, "alt-a"));
    assert!(shown(&o, v) && has_keys(&o, v));
    // the Later button dismisses too
    let later = b(v, "decision-later").center();
    v.simulate_click(later, Default::default());
    assert!(!shown(&o, v));
    // a NEW revision of the card opens it again, by itself, without the keys
    edit_card(&o, v, |c| c.revision += 1);
    assert!(shown(&o, v) && !has_keys(&o, v));
}

#[gpui::test]
fn a_click_on_the_card_gives_it_the_keys_and_an_option_click_chooses_without_sending(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    let opt = b(v, "decision-opt-1").center();
    v.simulate_click(opt, Default::default());
    assert!(has_keys(&o, v));
    assert_eq!(cursor(&o, v), Some(Some(1)));
    assert_eq!(answers(&o, v), 0, "choosing sends nothing");
}

#[gpui::test]
fn a_new_revision_while_focused_keeps_the_keys_clears_the_choice_and_rearms(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "3"));
    assert_eq!(cursor(&o, v), Some(Some(2)));
    edit_card(&o, v, |c| c.revision += 1);
    assert!(has_keys(&o, v), "still has the keys");
    assert_eq!(cursor(&o, v), Some(None), "the choice is gone");
    // armed again: Enter after a fresh choice inside the window sends nothing
    assert!(press(&o, v, "1"));
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 0);
}

#[gpui::test]
fn an_answer_in_flight_blocks_the_other_surface_and_a_reply_clears_the_choice(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a"));
    assert!(press(&o, v, "tab"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "down"));
    // the Decisions tab has an answer on its way: the card sends nothing, and says so
    o.update(v, |o, _| o.state.ask.sending = true);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 0);
    assert_eq!(
        notice(&o, v).as_deref(),
        Some("An answer is already on its way.")
    );
    o.update(v, |o, _| o.state.ask.sending = false);
    // now it sends (the sample answers at once with "nothing was sent", as a failed answer would)
    assert!(press(&o, v, "enter"));
    v.run_until_parked();
    assert_eq!(answers(&o, v), 1);
    // the reply cleared the choice: one more Enter resends nothing
    assert_eq!(
        cursor(&o, v),
        Some(None),
        "the choice is gone after the reply"
    );
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 1, "no resend after a reply");
}

#[gpui::test]
fn alt_a_goes_round_the_rows_and_a_session_that_waits_is_the_shells_to_open(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    let seen = Rc::new(RefCell::new(vec![]));
    let log = seen.clone();
    let sub = v.update(|_, cx| {
        cx.subscribe(&o, move |_, e: &OpenSession, _| {
            log.borrow_mut().push(e.0.clone())
        })
    });
    o.update(v, |o, cx| {
        o.set_waiting_tools(vec![("s1".into(), "refactor-auth".into())], cx)
    });
    v.run_until_parked();
    // the card is up: Alt+A moves the keys into it; Later; Alt+A then goes to the next row, the session
    let later = |v: &mut VisualTestContext| {
        let c = b(v, "decision-later").center();
        v.simulate_click(c, Default::default());
    };
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    later(v);
    assert!(press(&o, v, "alt-a"), "the oldest row first: the decision");
    assert!(shown(&o, v) && has_keys(&o, v));
    assert!(seen.borrow().is_empty());
    later(v);
    assert!(press(&o, v, "alt-a"));
    assert_eq!(
        seen.borrow().as_slice(),
        ["s1"],
        "then the session, in the shell"
    );
    drop(sub);
}

#[gpui::test]
fn on_the_decisions_tab_alt_a_selects_the_card_for_the_view_and_draws_no_second_one(
    cx: &mut TestAppContext,
) {
    let (o, v) = open(cx, BIG, CentreTab::Decisions);
    assert!(press(&o, v, "alt-a"));
    assert!(v.debug_bounds("decision").is_none());
    assert!(!has_keys(&o, v), "the view has the keys there");
    assert_eq!(o.read_with(v, |o, _| o.state.pane), Pane::Centre);
}

// ------------------------------------------------------------------ the keys never stay in the card by accident

/// What the card must not have done: chosen, sent, or kept the keys.
fn assert_view_has_the_keys(o: &Entity<OutcomesView>, v: &mut VisualTestContext, why: &str) {
    assert!(!has_keys(o, v), "{why}: the card still has the keys");
    for k in ["1", "enter"] {
        press(o, v, k);
        assert_eq!(cursor(o, v), Some(None), "{why}: {k} chose an option");
        assert_eq!(answers(o, v), 0, "{why}: {k} sent an answer");
    }
}

#[gpui::test]
fn a_click_elsewhere_and_the_window_going_to_the_background_take_the_keys_back(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    tick(&o, v, ARM_MS + 100);
    // a press outside the card (the list column)
    let left = b(v, "out-left");
    v.simulate_click(
        gpui::point(left.origin.x + px(20.), left.origin.y + px(300.)),
        Default::default(),
    );
    v.run_until_parked();
    assert!(shown(&o, v), "the card stays");
    tick(&o, v, ARM_MS + 100);
    assert_view_has_the_keys(&o, v, "after a click outside the card");
    // the window deactivating (or activating) gives them back as well
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    o.update_in(v, |o, window, cx| o.window_activation_changed(window, cx));
    v.run_until_parked();
    tick(&o, v, ARM_MS + 100);
    assert_view_has_the_keys(&o, v, "after the window changed activation");
}

#[gpui::test]
fn another_tab_or_another_outcome_gives_the_keys_back(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    o.update(v, |o, cx| {
        o.state.centre = CentreTab::History;
        cx.notify();
    });
    v.run_until_parked();
    tick(&o, v, ARM_MS + 100);
    assert_view_has_the_keys(&o, v, "after a tab switch");
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    o.update(v, |o, cx| {
        let other = o
            .state
            .items
            .iter()
            .find(|i| Some(&i.row.id) != o.state.selected.as_ref());
        let id = other.map(|i| i.row.id.clone()).unwrap();
        o.select_id(&id, cx);
    });
    v.run_until_parked();
    tick(&o, v, ARM_MS + 100);
    assert_view_has_the_keys(&o, v, "after an outcome switch");
}

#[gpui::test]
fn a_different_card_drops_the_keys_but_a_new_revision_of_the_same_one_keeps_them(
    cx: &mut TestAppContext,
) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a") && has_keys(&o, v));
    edit_card(&o, v, |c| c.revision += 1);
    assert!(has_keys(&o, v), "same card, new revision: keeps the keys");
    edit_card(&o, v, |c| c.id = "another_card".into());
    assert!(shown(&o, v));
    assert_view_has_the_keys(&o, v, "after a different card arrived");
}

// ------------------------------------------------------------------------------ collapsed and expanded card

fn open_card(
    cx: &mut TestAppContext,
    size: (f32, f32),
    expanded: bool,
) -> (Entity<OutcomesView>, &mut VisualTestContext) {
    let (o, v) = open(cx, size, CentreTab::Goal);
    if expanded {
        o.update_in(v, |o, window, cx| {
            o.focus_decision(window, cx);
            o.expand_decision(cx);
        });
        v.run_until_parked();
    }
    (o, v)
}

#[gpui::test]
fn collapsed_shows_the_first_field_and_says_how_many_are_not_shown(cx: &mut TestAppContext) {
    for size in [BIG, SMALL] {
        let (o, v) = open_card(cx, size, false);
        let n = o.read_with(v, |o, _| match o.pop.shown.as_ref() {
            Some(Shown::Card(p, _)) => p.card.rows.len(),
            _ => 0,
        });
        assert!(n > 1, "the sample has several sections");
        assert!(v.debug_bounds("decision-field-0").is_some());
        assert!(
            v.debug_bounds("decision-field-1").is_none(),
            "only the first field when collapsed"
        );
        assert!(v.debug_bounds("decision-more").is_some());
        let d = b(v, "decision");
        for sel in [
            "decision-opt-0",
            "decision-opt-4",
            "decision-send",
            "decision-later",
            "decision-hint",
        ] {
            assert!(
                b(v, sel).bottom() <= d.bottom(),
                "{size:?} {sel} is inside: {:?} vs card {d:?}",
                b(v, sel)
            );
        }
    }
}

#[gpui::test]
fn expanding_shows_every_field_in_order_scrolling_inside_with_the_answers_pinned(
    cx: &mut TestAppContext,
) {
    for size in [BIG, SMALL] {
        let (o, v) = open_card(cx, size, true);
        let n = o.read_with(v, |o, _| match o.pop.shown.as_ref() {
            Some(Shown::Card(p, _)) => p.card.rows.len(),
            _ => 0,
        });
        let (d, brief, centre) = (b(v, "decision"), b(v, "decision-brief"), b(v, "out-centre"));
        let mut last_y = f32::MIN;
        for i in 0..n {
            let sel: &'static str = Box::leak(format!("decision-field-{i}").into_boxed_str());
            let f = v.debug_bounds(sel);
            assert!(f.is_some(), "field {i} of {n} is in the brief");
            let y = f32::from(f.unwrap().origin.y);
            assert!(y >= last_y, "server order at {i}");
            last_y = y;
        }
        assert!(brief.bottom() <= d.bottom());
        assert!(d.bottom() <= centre.bottom(), "inside the column");
        assert!(
            d.origin.y >= b(v, "out-centre-body").origin.y,
            "under the tabs"
        );
        assert!(b(v, "decision-opt-4").bottom() <= d.bottom());
        assert!(b(v, "decision-send").bottom() <= d.bottom());
        // Tab (the card has the keys) collapses it again
        assert!(press(&o, v, "tab"));
        assert!(v.debug_bounds("decision-field-1").is_none());
    }
}

#[gpui::test]
fn the_card_height_is_capped_to_a_share_of_the_window(cx: &mut TestAppContext) {
    let (_, v) = open_card(cx, BIG, false);
    let collapsed = f32::from(b(v, "decision").size.height);
    let room = 900. - crate::ui::TITLEBAR_H - 56.;
    // 42% of the room, but never less than the answers, buttons and hint need
    assert!(
        collapsed <= (room * 0.42).max(380.) + 2.,
        "collapsed: {collapsed} of {room}"
    );
    assert!(collapsed >= room * 0.25, "and not tiny: {collapsed}");
    let (_, v) = open_card(cx, BIG, true);
    let expanded = f32::from(b(v, "decision").size.height);
    assert!(
        expanded <= (room * 0.6).max(420.) + 2.,
        "expanded: {expanded} of {room}"
    );
    assert!(
        expanded > collapsed,
        "expanded is taller than collapsed: {expanded}"
    );
}

#[gpui::test]
fn a_new_revision_collapses_the_brief_again(cx: &mut TestAppContext) {
    let (o, v) = open_card(cx, BIG, true);
    assert!(v.debug_bounds("decision-field-1").is_some());
    edit_card(&o, v, |c| c.revision += 1);
    assert!(
        v.debug_bounds("decision-field-1").is_none(),
        "collapsed again"
    );
}

// ------------------------------------------------------------------ fail closed on hidden sections

#[gpui::test]
fn send_needs_the_whole_brief_shown_once_per_card_and_revision(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    assert!(press(&o, v, "alt-a"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "2"));
    // collapsed with sections hidden: Enter sends nothing and says why
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 0);
    assert_eq!(
        notice(&o, v).as_deref(),
        Some("Expand the brief first (Tab): 7 sections not shown")
    );
    // the button is the same press: inert too
    let send = b(v, "decision-send").center();
    v.simulate_click(send, Default::default());
    assert_eq!(answers(&o, v), 0, "the Send button is inert too");
    // Tab expands: the choice stays; after the window, Enter sends once
    assert!(press(&o, v, "tab"));
    assert_eq!(cursor(&o, v), Some(Some(1)));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 1, "sent once after the brief was shown");
    // a new revision: collapsed again, expand required again
    edit_card(&o, v, |c| c.revision += 1);
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "2"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(
        answers(&o, v),
        1,
        "the new revision needs the brief shown again"
    );
    assert!(notice(&o, v).unwrap().starts_with("Expand the brief first"));
    // a card with nothing hidden sends without expanding (one section only)
    edit_card(&o, v, |c| {
        c.rows.truncate(1);
        c.revision += 1;
    });
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "2"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 2, "nothing hidden: no expand needed");
}

#[gpui::test]
fn a_single_long_field_is_cut_so_it_needs_the_expand_before_send(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    edit_card(&o, v, |c| {
        c.rows.truncate(1);
        c.rows[0].text = "long ".repeat(60);
        c.revision += 1;
    });
    assert!(press(&o, v, "alt-a"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "2"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 0, "the cut field blocks Enter");
    assert_eq!(
        notice(&o, v).as_deref(),
        Some("Expand the brief first (Tab): text is cut")
    );
    let send = b(v, "decision-send").center();
    v.simulate_click(send, Default::default());
    assert_eq!(answers(&o, v), 0, "and the Send button");
    assert!(press(&o, v, "tab"));
    tick(&o, v, ARM_MS + 100);
    assert!(press(&o, v, "enter"));
    assert_eq!(answers(&o, v), 1, "sent after the field was shown whole");
}

#[gpui::test]
fn a_held_alt_a_does_not_flip_the_keys_back_and_forth(cx: &mut TestAppContext) {
    let (o, v) = open_goal(cx);
    let held = gpui::KeyDownEvent {
        keystroke: gpui::Keystroke::parse("alt-a").unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    let r = o.update_in(v, |o, window, cx| o.on_key(&held, window, cx));
    assert!(r, "consumed");
    assert!(!has_keys(&o, v), "a held key does nothing");
}

// ------------------------------------------------------------------ back and forward, the rail

#[gpui::test]
fn alt_left_and_alt_right_walk_the_outcomes_shown_and_the_buttons_follow(cx: &mut TestAppContext) {
    let (o, v) = open(cx, BIG, CentreTab::Goal);
    let first = o.read_with(v, |o, _| o.state.selected.clone()).unwrap();
    assert!(
        !o.read_with(v, |o, _| o.can_back() || o.can_forward()),
        "one place: nowhere to go"
    );
    let second = o.read_with(v, |o, _| {
        o.state
            .items
            .iter()
            .map(|i| i.row.id.clone())
            .find(|i| *i != first)
            .unwrap()
    });
    o.update(v, |o, cx| o.select_id(&second, cx));
    v.run_until_parked();
    assert_eq!(
        o.read_with(v, |o, _| o.state.selected.clone()),
        Some(second.clone())
    );
    assert!(o.read_with(v, |o, _| o.can_back() && !o.can_forward()));
    assert!(press(&o, v, "alt-left"));
    assert_eq!(
        o.read_with(v, |o, _| o.state.selected.clone()),
        Some(first.clone())
    );
    assert!(o.read_with(v, |o, _| !o.can_back() && o.can_forward()));
    assert!(press(&o, v, "alt-right"));
    assert_eq!(
        o.read_with(v, |o, _| o.state.selected.clone()),
        Some(second)
    );
    // going back and forward is not a new visit: nothing ahead was lost
    assert!(press(&o, v, "alt-left") && press(&o, v, "alt-left"));
    assert_eq!(o.read_with(v, |o, _| o.state.selected.clone()), Some(first));
}

#[gpui::test]
fn the_rail_button_hides_the_list_column_and_the_centre_takes_the_room(cx: &mut TestAppContext) {
    let (o, v) = open(cx, BIG, CentreTab::Goal);
    let (l, c) = (b(v, "out-left"), b(v, "out-centre"));
    o.update(v, |o, cx| o.toggle_rail(cx));
    v.run_until_parked();
    assert!(v.debug_bounds("out-left").is_none());
    let c2 = b(v, "out-centre");
    assert!(
        c2.size.width > c.size.width + l.size.width - px(1.),
        "{c:?} -> {c2:?}"
    );
    assert!(c2.origin.x < c.origin.x);
    o.update(v, |o, cx| o.toggle_rail(cx));
    v.run_until_parked();
    assert_eq!(b(v, "out-left").size.width, l.size.width, "and back");
}

#[test]
fn the_tui_view_has_no_answer_path_for_a_decision_any_more() {
    // the grep test of S0: nothing of the decision card, the approvals list or the answer worker is in the TUI view
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui_view");
    let banned = [
        "send_effect",
        "Effect::Resolve",
        "outcomes_view",
        "crate::approvals",
        "OpenOutcome",
        "ChatTab::Outcomes",
    ];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            let text = std::fs::read_to_string(&p).unwrap();
            for b in banned {
                assert!(!text.contains(b), "{} mentions {b}", p.display());
            }
        }
    }
}
