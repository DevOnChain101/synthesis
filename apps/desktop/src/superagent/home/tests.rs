use gpui::{
    Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use serde_json::json;
use synthesis_view::outcomes::{DecisionCard, uncovered};

use super::card::{more_label, shown_rows, why_not};
use super::data::{self, Gates, Risk, Target};
use super::*;
use crate::approvals::{Ages, Popup, decision_rows};
use crate::outcomes_view::popup::SAMPLE_NOW;
use crate::outcomes_view::sample;

fn sample_model() -> Model {
    let st = sample::state();
    let mut ages = Ages::default();
    ages.seed("o_auth", SAMPLE_NOW - 9 * 60_000);
    data::build(&st, decision_rows(&st.items, &ages, SAMPLE_NOW), SAMPLE_NOW)
}

#[test]
fn the_sections_are_needs_running_finished_and_nothing_is_dropped() {
    let m = sample_model();
    assert_eq!(m.needs.len(), 1);
    assert_eq!(m.needs[0].age, "9m");
    assert_eq!(m.needs[0].detail, "2 decisions");
    let ids = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
    assert_eq!(
        ids(m.running.iter().map(|r| r.id.as_str()).collect()),
        ["o_auth", "o_ci", "o_cfg"]
    );
    assert_eq!(m.finished.len(), 1, "the verified one");
    assert_eq!(m.finished[0].notes, ["Waiting for your acknowledgement"]);
    assert!(m.running[0].notes[0].contains("2 decisions"));
    assert_eq!(m.running[2].status, Status::Blocked);
    assert_eq!(
        m.flat(),
        [
            Target::Need(0),
            Target::Run(0),
            Target::Run(1),
            Target::Run(2),
            Target::Fin(0)
        ]
    );
    assert!(!m.is_empty());
    assert!(Model::default().is_empty());
}

#[test]
fn the_recommended_one_tap_is_gated_off_and_never_for_high_risk() {
    let mut r = sample_model().needs.remove(0);
    let open = Gates {
        one_tap: true,
        ..Gates::SHIPPED
    };
    assert_eq!(Gates::SHIPPED, Gates::default());
    assert!(!r.one_tap(open), "the model carries no risk yet");
    r.risk = Some(Risk::Low);
    r.recommended = Some("keep".into());
    assert!(r.one_tap(open));
    assert!(!r.one_tap(Gates::SHIPPED), "the gate is shut");
    r.risk = Some(Risk::High);
    assert!(!r.one_tap(open), "high risk never");
}

#[test]
fn the_keyboard_map() {
    assert_eq!(act("j", false, false), Some(Act::Down));
    assert_eq!(act("down", false, false), Some(Act::Down));
    assert_eq!(act("k", false, false), Some(Act::Up));
    assert_eq!(act("enter", false, false), Some(Act::Open));
    assert_eq!(
        act("r", false, false),
        None,
        "R is Retry only with the banner"
    );
    assert_eq!(act("r", true, false), Some(Act::Retry));
    assert_eq!(act("2", false, false), None);
    assert_eq!(act("2", false, true), Some(Act::Kind(1)));
    assert_eq!(act("x", true, true), None);
}

fn brief_card() -> DecisionCard {
    DecisionCard::from_json(&json!({"id": "d1", "kind": "ambiguity", "state": "pending", "revision": 3,
        "brief_sha256": "c".repeat(64), "expires_at": "2030-01-01T00:00:00.000Z",
        "brief": {"why_human": "line one\nline two\nline three\nline four", "alternatives": [{"id": "A", "label": "first"}, {"id": "B", "label": "second"}],
                  "unknowns": ["is it typed?"], "future_field": "shown even so"}}))
    .unwrap()
}

#[test]
fn the_brief_hides_fields_until_opened_and_then_shows_every_one() {
    let c = brief_card();
    let mut p = Popup::new("o".into(), c.clone(), 0);
    assert!(p.send_blocked(), "text is hidden");
    assert!(shown_rows(&p).len() < c.rows.len());
    assert_eq!(more_label(&p).as_deref(), Some("Show 3 more sections"));
    assert_eq!(why_not(&p, 10_000).as_deref(), Some("Choose an answer"));
    p.ask.choose(0);
    assert_eq!(
        why_not(&p, 10_000).as_deref(),
        Some("Open the whole brief first")
    );
    p.toggle_full();
    assert_eq!(why_not(&p, 10_000), None);
    let all: String = shown_rows(&p)
        .iter()
        .map(|r| format!("{}\n{}\n", r.label, r.text))
        .collect();
    let brief = json!({"why_human": "line one\nline two\nline three\nline four",
        "alternatives": [{"id": "A", "label": "first"}, {"id": "B", "label": "second"}],
        "unknowns": ["is it typed?"], "future_field": "shown even so"});
    assert_eq!(uncovered(&brief, &all), Vec::<String>::new(), "{all}");
    assert_eq!(more_label(&p).as_deref(), Some("Show less"));
}

fn open(
    cx: &mut TestAppContext,
) -> (
    Entity<HomeView>,
    Entity<OutcomesView>,
    &mut VisualTestContext,
) {
    cx.update(gpui_component::init);
    let (h, v) = cx.add_window_view(|window, cx| {
        let o = cx.new(OutcomesView::sample);
        o.update(cx, |o, _| o.sample_clock());
        let mut h = HomeView::new(o, cx);
        h.shown(window, cx);
        h
    });
    v.simulate_resize(size(px(900.), px(900.)));
    v.run_until_parked();
    let o = h.read_with(v, |h, _| h.outcomes.clone());
    (h, o, v)
}

fn press(v: &mut VisualTestContext, h: &Entity<HomeView>, key: &str, m: Modifiers) -> bool {
    let ev = KeyDownEvent {
        keystroke: Keystroke {
            modifiers: m,
            key: key.into(),
            key_char: None,
        },
        is_held: false,
        prefer_character_input: false,
    };
    let r = h.update(v, |h, cx| h.on_key(&ev, cx));
    v.run_until_parked();
    r
}

fn alt() -> Modifiers {
    Modifiers {
        alt: true,
        ..Default::default()
    }
}

fn clock(v: &mut VisualTestContext, o: &Entity<OutcomesView>, ms: i64) {
    o.update(v, |o, _| o.pop.fixed_now = Some(ms));
}

fn chosen(v: &mut VisualTestContext, h: &Entity<HomeView>) -> Option<Option<usize>> {
    h.read_with(v, |h, _| {
        h.open
            .as_ref()?
            .popup
            .as_ref()
            .and_then(|p| p.ask.picker.as_ref().map(|k| k.cursor()))
    })
}

#[gpui::test]
fn review_opens_the_card_in_place_of_the_row_with_nothing_preselected(cx: &mut TestAppContext) {
    let (h, _o, v) = open(cx);
    assert!(v.debug_bounds("sa-home").is_some());
    assert!(v.debug_bounds("home-needs-sec").is_some());
    assert!(v.debug_bounds("home-need-0").is_some(), "the row");
    assert!(v.debug_bounds("home-card").is_none());
    h.update(v, |h, cx| h.review("o_auth", cx));
    v.run_until_parked();
    assert!(v.debug_bounds("home-card").is_some(), "the card");
    assert!(
        v.debug_bounds("home-need-0").is_none(),
        "in place of the row"
    );
    assert_eq!(chosen(v, &h), Some(None), "nothing preselected");
    assert!(v.debug_bounds("home-opt-0").is_some());
    // Esc closes it, the row is back
    assert!(press(v, &h, "escape", Modifiers::default()));
    assert!(v.debug_bounds("home-card").is_none());
    assert!(v.debug_bounds("home-need-0").is_some());
}

#[gpui::test]
fn the_card_keys_wait_400ms_and_send_needs_a_choice_and_an_opened_brief(cx: &mut TestAppContext) {
    let (h, o, v) = open(cx);
    h.update(v, |h, cx| h.review("o_auth", cx));
    v.run_until_parked();
    let none = Modifiers::default();
    // inside the guard: a digit chooses nothing
    clock(v, &o, SAMPLE_NOW + 100);
    press(v, &h, "1", none);
    assert_eq!(chosen(v, &h), Some(None));
    clock(v, &o, SAMPLE_NOW + 500);
    press(v, &h, "1", none);
    assert_eq!(chosen(v, &h), Some(Some(0)), "a choice, not a send");
    let answers = o.read_with(v, |o, _| o.state.answers);
    press(v, &h, "enter", none);
    let hidden = h.read_with(v, |h, _| {
        h.open
            .as_ref()
            .unwrap()
            .popup
            .as_ref()
            .unwrap()
            .send_blocked()
    });
    let after = o.read_with(v, |o, _| o.state.answers);
    if hidden {
        assert_eq!(after, answers, "Send is refused while text is hidden");
        press(v, &h, "tab", none);
        press(v, &h, "enter", none);
    }
    // the sample answers locally and sends nothing: its reply is counted
    assert_eq!(o.read_with(v, |o, _| o.state.answers), answers + 1);
}

#[gpui::test]
fn alt_a_walks_the_waiting_rows_and_j_k_move_the_cursor(cx: &mut TestAppContext) {
    let (h, _o, v) = open(cx);
    assert!(press(v, &h, "j", Modifiers::default()));
    assert_eq!(h.read_with(v, |h, _| h.cursor), Some(0));
    assert!(press(v, &h, "j", Modifiers::default()));
    assert_eq!(h.read_with(v, |h, _| h.cursor), Some(1));
    assert!(press(v, &h, "k", Modifiers::default()));
    assert_eq!(h.read_with(v, |h, _| h.cursor), Some(0));
    assert!(press(v, &h, "a", alt()));
    assert!(
        v.debug_bounds("home-card").is_some(),
        "Alt+A opens the oldest decision inline"
    );
}

#[gpui::test]
fn loading_error_and_empty_states(cx: &mut TestAppContext) {
    let (h, o, v) = open(cx);
    o.update(v, |o, cx| {
        o.state.set_screen(Screen::Loading);
        cx.notify();
    });
    v.run_until_parked();
    assert!(v.debug_bounds("home-skeleton").is_some());
    o.update(v, |o, cx| {
        o.state
            .set_screen(Screen::Unreachable("no endpoint".into()));
        cx.notify();
    });
    v.run_until_parked();
    assert!(v.debug_bounds("home-banner").is_some());
    assert!(v.debug_bounds("home-retry").is_some());
    assert!(press(v, &h, "r", Modifiers::default()), "R retries");
    assert!(
        !o.read_with(v, |o, _| o.state.ask.sending),
        "a retry is not an answer"
    );
    o.update(v, |o, cx| {
        o.state.set_screen(Screen::Ready);
        o.state.items.clear();
        cx.notify();
    });
    v.run_until_parked();
    assert!(v.debug_bounds("home-empty").is_some());
    assert!(v.debug_bounds("home-kind-0").is_some());
    assert_eq!(
        o.read_with(v, |o, _| o.state.picker.clone()),
        None,
        "nothing is open yet"
    );
    assert!(press(v, &h, "2", Modifiers::default()));
    assert_eq!(
        o.read_with(v, |o, _| o.state.picker.as_ref().map(|p| p.cursor)),
        Some(Some(1)),
        "the picker rows open with the kind the person chose"
    );
}

#[gpui::test]
fn the_conversation_slot_is_at_the_bottom(cx: &mut TestAppContext) {
    let (_h, _o, v) = open(cx);
    let slot = v.debug_bounds("home-conversation").unwrap();
    let fin = v.debug_bounds("home-finished-sec").unwrap();
    assert!(slot.origin.y >= fin.origin.y + fin.size.height - px(1.));
}

#[gpui::test]
fn the_conversation_slot_is_there_when_the_home_is_empty(cx: &mut TestAppContext) {
    let (_h, o, v) = open(cx);
    o.update(v, |o, cx| {
        o.state.set_screen(Screen::Ready);
        o.state.items.clear();
        cx.notify();
    });
    v.run_until_parked();
    assert!(v.debug_bounds("home-empty").is_some());
    assert!(
        v.debug_bounds("home-conversation").is_some(),
        "the chat must not vanish on a fresh home"
    );
}
