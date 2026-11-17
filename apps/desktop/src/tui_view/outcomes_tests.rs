//! Geometry and key tests of the outcome parts of the TUI view (tab, Approvals panel, decision popup) on the
//! sample data, and of the guarantee that none of it exists while outcomes are off. The tests of `design_tests`
//! (measured against `cli.html`) run with no outcomes link, so they also prove the off state changes no geometry.
//!
//! The key model under test: the popup opens by itself and never takes the keys; Alt+A (or a click) gives them to
//! it; Esc or Alt+A gives them back; a second Esc soon after, or "Later", dismisses it.

use gpui::{AppContext as _, Bounds, Pixels, TestAppContext, VisualTestContext, px, size};

use super::TuiView;
use super::approvals_ui::{OutShot, Shown};
use super::layout::Hidden;
use super::sample::OUTCOMES_NOW;
use crate::live::ARM_MS;
use crate::outcomes_view::OutcomesView;

fn open(
    cx: &mut TestAppContext,
    (w, h): (f32, f32),
    what: Option<OutShot>,
    hidden: Hidden,
) -> (gpui::Entity<TuiView>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (t, v) = cx.add_window_view(|_, cx| {
        let mut t = TuiView::new(0., hidden);
        if let Some(what) = what {
            let ov = cx.new(OutcomesView::sample);
            t.seed_sample(what, ov, cx);
        }
        t
    });
    v.simulate_resize(size(px(w), px(h)));
    v.run_until_parked();
    (t, v)
}

fn b(v: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
    v.debug_bounds(sel)
        .unwrap_or_else(|| panic!("no element {sel}"))
}

fn press(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, k: &'static str) -> bool {
    let ks = gpui::Keystroke::parse(k).unwrap();
    let r = t.update_in(v, |t, window, cx| t.on_key(&ks, window, cx));
    v.run_until_parked();
    r
}

const BIG: (f32, f32) = (1400., 900.);
const SMALL: (f32, f32) = (1100., 700.);

#[gpui::test]
fn nothing_exists_while_outcomes_are_off(cx: &mut TestAppContext) {
    let (t, v) = open(cx, BIG, None, Hidden::default());
    assert!(t.read_with(v, |t, _| t.outcomes.is_none()));
    assert!(v.debug_bounds("p-approvals").is_none());
    assert!(v.debug_bounds("decision").is_none());
    assert!(
        v.debug_bounds("tab-5").is_none(),
        "five tabs, as the design"
    );
    assert!(v.debug_bounds("tab-4").is_some());
    // the right column starts with Topology, as before
    assert_eq!(
        f32::from(b(v, "p-topology").origin.y),
        f32::from(b(v, "col-right").origin.y)
    );
    // the keys are not ours: nothing is consumed, nothing opens
    assert!(!press(&t, v, "alt-0") && !press(&t, v, "alt-a"));
    assert!(t.read_with(v, |t, _| t.ts.tab == super::ChatTab::Chat));
}

#[gpui::test]
fn the_approvals_panel_is_on_top_of_the_right_column_and_oldest_first(cx: &mut TestAppContext) {
    let (t, v) = open(cx, BIG, Some(OutShot::Approvals3), Hidden::default());
    let (col, p, topo) = (b(v, "col-right"), b(v, "p-approvals"), b(v, "p-topology"));
    assert_eq!(f32::from(p.origin.y), f32::from(col.origin.y), "on top");
    assert_eq!(p.size.width, col.size.width);
    assert!(topo.origin.y >= p.bottom(), "Topology is below it");
    for sel in ["appr-row-0", "appr-row-1", "appr-row-2"] {
        assert!(v.debug_bounds(sel).is_some(), "{sel}");
    }
    // tool ask (14 min), decision (9 min), tool ask (2 min): the longest waiting first
    let kinds: Vec<_> = t.update(v, |t, cx| {
        t.approval_rows(cx)
            .into_iter()
            .map(|r| r.kind.label())
            .collect()
    });
    assert_eq!(kinds, ["Tool", "Decision", "Tool"]);
    // one row is a shorter panel
    let (_, v1) = open(cx, BIG, Some(OutShot::Approvals1), Hidden::default());
    assert!(b(v1, "p-approvals").size.height < p.size.height);
    assert!(v1.debug_bounds("appr-row-1").is_none());
}

#[gpui::test]
fn the_approvals_panel_can_be_switched_off_and_is_a_drawer_on_a_narrow_window(
    cx: &mut TestAppContext,
) {
    let mut h = Hidden::default();
    h.set_hidden("approvals", true);
    let (_, v) = open(cx, BIG, Some(OutShot::Approvals3), h);
    assert!(
        v.debug_bounds("p-approvals").is_none(),
        "/panels approvals off"
    );
    // below 1280 the right column is the slide-over it always was; the panel is in it
    let (t, v) = open(cx, SMALL, Some(OutShot::Approvals3), Hidden::default());
    assert!(v.debug_bounds("col-right").is_none());
    t.update(v, |t, cx| t.toggle_drawer(super::Side::Right, cx));
    v.run_until_parked();
    assert!(v.debug_bounds("p-approvals").is_some());
}

fn focus_of(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> bool {
    t.update_in(v, |t, window, _| t.popup_has_keys(window))
}

fn popup_cursor(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> Option<Option<usize>> {
    t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.ask.picker.as_ref().map(|p| p.cursor()),
            _ => None,
        }
    })
}

fn shown(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> bool {
    t.read_with(v, |t, _| t.outcomes.as_ref().unwrap().shown.is_some())
}

fn answers(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> u64 {
    t.read_with(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.read(cx).state.answers
    })
}

/// The clock of the view moves on by `ms`.
fn tick(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, ms: i64) {
    t.update(v, |t, _| {
        let w = t.outcomes.as_mut().unwrap();
        w.fixed_now = w.fixed_now.map(|n| n + ms);
    });
}

fn set_prompt(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, s: &'static str) {
    t.update_in(v, |t, window, cx| t.set_prompt(s, window, cx));
    v.run_until_parked();
}

#[gpui::test]
fn the_decision_popup_opens_by_itself_above_the_prompt_without_the_keys(cx: &mut TestAppContext) {
    for size in [BIG, SMALL] {
        let (t, v) = open(cx, size, Some(OutShot::Decision), Hidden::default());
        let (d, prompt, chat) = (b(v, "decision"), b(v, "prompt"), b(v, "p-chat"));
        assert!(d.bottom() <= prompt.origin.y, "above the prompt");
        assert!(d.origin.y >= chat.origin.y && d.bottom() <= chat.bottom());
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
        assert!(!focus_of(&t, v), "it never takes the keys by itself");
        assert_eq!(popup_cursor(&t, v), Some(None), "no preselection");
    }
}

#[gpui::test]
fn the_focused_and_the_unfocused_popup_differ_in_the_hint_and_say_which_keys_are_where(
    cx: &mut TestAppContext,
) {
    let (t, v) = open(cx, BIG, Some(OutShot::Decision), Hidden::default());
    assert!(v.debug_bounds("decision-hint").is_some());
    assert!(!focus_of(&t, v));
    let (t2, v2) = open(cx, BIG, Some(OutShot::DecisionFocused), Hidden::default());
    t2.update_in(v2, |t, window, cx| t.focus_decision(window, cx));
    v2.run_until_parked();
    assert!(focus_of(&t2, v2));
    assert!(v2.debug_bounds("decision").is_some());
}

/// A live view (no engine) with the Decision sample open (by itself), the clock past the arming delay.
fn open_live(cx: &mut TestAppContext) -> (gpui::Entity<TuiView>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (t, v) = cx.add_window_view(|window, cx| {
        let mut t = TuiView::live(window, cx, super::tests::live_cfg(), Hidden::default(), 0.);
        let mut l = crate::live::Live::new();
        l.starting = false;
        t.set_live(l);
        let ov = cx.new(OutcomesView::sample);
        t.seed_sample(OutShot::Decision, ov, cx);
        t
    });
    v.simulate_resize(size(px(1400.), px(900.)));
    v.run_until_parked();
    tick(&t, v, 10_000);
    (t, v)
}

#[gpui::test]
fn an_auto_opened_popup_never_touches_the_prompt_or_its_keys(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    assert!(shown(&t, v) && !focus_of(&t, v));
    set_prompt(&t, v, "half a message");
    // every plain key goes to the prompt and the engine: nothing is consumed for the popup, nothing is chosen
    // or sent, and the text is untouched by them (up and down are the engine's prompt history)
    for k in [
        "1",
        "2",
        "9",
        "enter",
        "tab",
        "escape",
        "shift-enter",
        "shift-2",
        "shift-3",
        "shift-1",
        "shift-tab",
        "up",
        "down",
    ] {
        set_prompt(&t, v, "half a message");
        let consumed = press(&t, v, k);
        assert!(
            !consumed || matches!(k, "up" | "down" | "escape" | "tab" | "enter" | "shift-tab"),
            "{k} must reach the prompt"
        );
        assert_eq!(popup_cursor(&t, v), Some(None), "{k} chose nothing");
        assert_eq!(answers(&t, v), 0, "{k} sent nothing");
        assert!(shown(&t, v), "{k} did not close it");
        assert!(!focus_of(&t, v), "{k} did not focus it");
    }
    // typing while it is open lands in the prompt, and the popup is still there
    set_prompt(&t, v, "typed on");
    let text = t.read_with(v, |t, cx| {
        t.live.as_ref().unwrap().prompt.read(cx).value().to_string()
    });
    assert_eq!(text, "typed on");
}

#[gpui::test]
fn alt_a_gives_the_popup_the_keys_choose_then_enter_sends_and_esc_gives_them_back(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    set_prompt(&t, v, "draft");
    // Alt+A: the keys move into the popup; the draft stays in the prompt
    assert!(press(&t, v, "alt-a"));
    assert!(focus_of(&t, v));
    let text = |t: &gpui::Entity<TuiView>, v: &mut VisualTestContext| {
        t.read_with(v, |t, cx| {
            t.live.as_ref().unwrap().prompt.read(cx).value().to_string()
        })
    };
    assert_eq!(text(&t, v), "draft");
    // the whole brief has to be shown once before an answer can be sent (the sample hides sections when collapsed)
    assert!(press(&t, v, "tab"));
    // Enter right after the move is inert (ARM_MS), whatever was chosen
    assert!(press(&t, v, "down"));
    assert_eq!(popup_cursor(&t, v), Some(Some(0)));
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0, "inside the arming window after the move");
    // step 1: a number chooses (and sends nothing), step 2: Enter after the window sends
    assert!(press(&t, v, "2"));
    assert_eq!(popup_cursor(&t, v), Some(Some(1)));
    assert_eq!(answers(&t, v), 0);
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 1, "sent after a choice and the window");
    assert_eq!(text(&t, v), "draft", "the draft was never touched");
    // Tab again collapses it, and having seen it all once, Send stays allowed for this card
    assert!(press(&t, v, "tab"));
    assert!(t.read_with(v, |t, _| matches!(
        t.outcomes.as_ref().and_then(|w| w.shown.as_ref()),
        Some(Shown::Card(p, _)) if !p.full && p.seen_all && !p.send_blocked()
    )));
    // Esc gives the keys back and leaves the popup
    assert!(press(&t, v, "escape"));
    assert!(!focus_of(&t, v) && shown(&t, v));
    // Alt+A again takes them, and a third time gives them back without closing
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    assert!(press(&t, v, "alt-a") && !focus_of(&t, v) && shown(&t, v));
}

#[gpui::test]
fn a_second_esc_or_later_dismisses_it_until_a_new_card_or_alt_a(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    // Esc from the popup returns the keys; a second one soon after dismisses
    assert!(press(&t, v, "alt-a"));
    assert!(press(&t, v, "escape"));
    assert!(shown(&t, v));
    // an immediate second Esc is the first one auto-repeating: swallowed, nothing dismissed
    assert!(press(&t, v, "escape"));
    assert!(shown(&t, v), "a held Esc does not dismiss");
    tick(&t, v, 200);
    assert!(press(&t, v, "escape"));
    assert!(!shown(&t, v) && !focus_of(&t, v));
    assert!(v.debug_bounds("decision").is_none());
    // it stays closed for the same card (a refresh does not bring it back)...
    t.update(v, |_, cx| cx.notify());
    v.run_until_parked();
    assert!(!shown(&t, v));
    // ...and Alt+A brings it back, with the keys
    assert!(press(&t, v, "alt-a"));
    assert!(shown(&t, v) && focus_of(&t, v));
    // the Later button dismisses too
    let later = b(v, "decision-later").center();
    v.simulate_click(later, Default::default());
    assert!(!shown(&t, v));
    // a NEW revision of the card opens it again, by itself, without the keys
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    assert!(shown(&t, v) && !focus_of(&t, v));
}

#[gpui::test]
fn an_unfocused_esc_still_reaches_the_engine_and_a_click_gives_the_popup_the_keys(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    // a lone Esc with the prompt focused is not the popup's (it would interrupt a task, leave a tab, ...)
    let consumed = press(&t, v, "escape");
    assert!(
        shown(&t, v),
        "one Esc does not dismiss: consumed={consumed}"
    );
    // a click on the card gives it the keys, and an option click chooses (two steps: Send is separate)
    let opt = b(v, "decision-opt-1").center();
    v.simulate_click(opt, Default::default());
    assert!(focus_of(&t, v));
    assert_eq!(popup_cursor(&t, v), Some(Some(1)));
    assert_eq!(answers(&t, v), 0, "choosing sends nothing");
}

#[gpui::test]
fn a_new_revision_while_focused_keeps_the_keys_clears_the_choice_and_rearms(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "3"));
    assert_eq!(popup_cursor(&t, v), Some(Some(2)));
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    assert!(focus_of(&t, v), "still has the keys");
    assert_eq!(popup_cursor(&t, v), Some(None), "the choice is gone");
    // armed again: Enter after a fresh choice inside the window sends nothing
    assert!(press(&t, v, "1"));
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0);
}

#[gpui::test]
fn an_answer_in_flight_blocks_the_other_surface_and_a_reply_clears_the_choice(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a"));
    assert!(press(&t, v, "tab"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "down"));
    // the Outcomes tab has an answer on its way: the popup sends nothing, and says so
    t.update(v, |t, cx| {
        t.outcomes
            .as_ref()
            .unwrap()
            .view
            .update(cx, |o, _| o.state.ask.sending = true)
    });
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0);
    let note = t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => (p.ask.sending, p.ask.notice.clone()),
            _ => panic!("no card"),
        }
    });
    assert_eq!(
        note,
        (false, Some("An answer is already on its way.".into()))
    );
    t.update(v, |t, cx| {
        t.outcomes
            .as_ref()
            .unwrap()
            .view
            .update(cx, |o, _| o.state.ask.sending = false)
    });
    // now it sends (the sample answers at once with "nothing was sent", as a failed answer would)
    assert!(press(&t, v, "enter"));
    v.run_until_parked();
    assert_eq!(answers(&t, v), 1);
    // the reply cleared the choice: one more Enter resends nothing
    assert_eq!(
        popup_cursor(&t, v),
        Some(None),
        "the choice is gone after the reply"
    );
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 1, "no resend after a reply");
}

#[gpui::test]
fn other_overlays_keep_their_keys_and_a_tool_dialog_takes_the_focus_and_rearms(
    cx: &mut TestAppContext,
) {
    use crate::live::{Dialog, DialogKind};
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a"));
    // the history search (Ctrl+R) owns Esc while it is open: the popup keeps its place and its keys
    t.update(v, |t, _| {
        t.live.as_mut().unwrap().search = Some((String::new(), 0))
    });
    assert!(press(&t, v, "escape"), "the search took it");
    assert!(t.read_with(v, |t, _| t.live.as_ref().unwrap().search.is_none()));
    assert!(
        shown(&t, v) && focus_of(&t, v),
        "Esc closed the search only"
    );
    // a tool dialog wins, as today: the popup loses the keys, the dialog's Esc rejects the dialog and not the popup
    t.update(v, |t, cx| {
        t.live.as_mut().unwrap().live.dialogs.push(Dialog {
            kind: DialogKind::Plan {
                id: "p".into(),
                plan: "do it".into(),
            },
            options: vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
        cx.notify();
    });
    v.run_until_parked();
    assert!(!focus_of(&t, v), "the dialog took the keys");
    assert!(
        v.debug_bounds("decision").is_none(),
        "the dock is drawn instead"
    );
    assert!(press(&t, v, "escape"), "the dialog rejected");
    assert!(t.read_with(v, |t, _| t.live.as_ref().unwrap().live.dialogs.is_empty()));
    assert!(shown(&t, v), "the dialog's Esc did not close the popup");
    let armed = t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.ask.armed_until,
            _ => 0,
        }
    });
    assert!(armed >= OUTCOMES_NOW + 10_000 + ARM_MS);
    // so a choice and an Enter right after the dialog closed, with the keys taken again, send nothing
    assert!(press(&t, v, "alt-a"));
    assert!(press(&t, v, "down"));
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0);
}

#[gpui::test]
fn alt_a_goes_to_the_next_approval_and_alt_0_to_the_outcomes_tab(cx: &mut TestAppContext) {
    // approvals-1 has the popup put off: Alt+A brings it back with the keys
    let (t, v) = open(cx, BIG, Some(OutShot::Approvals1), Hidden::default());
    assert!(v.debug_bounds("decision").is_none());
    assert!(press(&t, v, "alt-a"));
    assert!(v.debug_bounds("decision").is_some() && focus_of(&t, v));
    assert!(press(&t, v, "escape"));
    tick(&t, v, 200);
    assert!(press(&t, v, "escape"));
    assert!(press(&t, v, "alt-0"));
    assert!(t.read_with(v, |t, _| t.ts.tab == super::ChatTab::Outcomes));
    assert!(v.debug_bounds("tab-5").is_some());
    assert!(v.debug_bounds("out-tab-row-0").is_some());
}

#[gpui::test]
fn the_outcomes_tab_lists_every_outcome_with_its_state_and_budget(cx: &mut TestAppContext) {
    let (_, v) = open(cx, BIG, Some(OutShot::Tab), Hidden::default());
    for sel in [
        "out-tab-row-0",
        "out-tab-row-1",
        "out-tab-row-2",
        "out-tab-row-3",
    ] {
        let r = b(v, sel);
        assert!(r.size.height > px(30.), "{sel}");
    }
    let (tabs, body) = (b(v, "tabs"), b(v, "tab-body"));
    assert!(body.origin.y >= tabs.bottom());
}

#[gpui::test]
fn bidi_controls_in_what_waits_for_you_are_shown_escaped(cx: &mut TestAppContext) {
    let (t, v) = open(cx, BIG, Some(OutShot::Approvals1), Hidden::default());
    let bidi = "\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
    t.update(v, |t, cx| {
        t.snap.running[0].state = super::activity::RunState::Approval;
        t.snap.running[0].title = format!("a{bidi}b");
        let rows = t.approval_rows(cx);
        for r in &rows {
            assert!(
                !r.subject.chars().any(|c| bidi.contains(c)),
                "raw bidi control in {:?}",
                r.subject
            );
        }
        assert!(rows.iter().any(|r| r.subject.contains("\\u{202e}")));
    });
}

// ------------------------------------------------------------------ the keys never stay in the popup by accident

/// The prompt's own bounds, for a click that goes to type there.
fn click_prompt(v: &mut VisualTestContext) {
    let c = b(v, "prompt").center();
    v.simulate_click(c, Default::default());
    v.run_until_parked();
}

/// What the popup must not have done: chosen, sent, or kept the keys.
fn assert_prompt_has_the_keys(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, why: &str) {
    assert!(!focus_of(t, v), "{why}: the popup still has the keys");
    for k in ["1", "enter", "down"] {
        press(t, v, k);
        assert_eq!(popup_cursor(t, v), Some(None), "{why}: {k} chose an option");
        assert_eq!(answers(t, v), 0, "{why}: {k} sent an answer");
    }
}

#[gpui::test]
fn a_click_in_the_prompt_takes_the_keys_back_so_typing_never_answers(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    tick(&t, v, ARM_MS + 100);
    click_prompt(v);
    assert!(shown(&t, v), "the popup stays");
    tick(&t, v, ARM_MS + 100);
    assert_prompt_has_the_keys(&t, v, "after a click in the prompt");
}

#[gpui::test]
fn a_click_elsewhere_in_the_view_and_the_window_going_to_the_background_do_too(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    // a press outside the popup (the chat area above it)
    let chat = b(v, "p-chat");
    v.simulate_click(
        gpui::point(chat.origin.x + px(40.), chat.origin.y + px(60.)),
        Default::default(),
    );
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert_prompt_has_the_keys(&t, v, "after a click outside the popup");
    // the window deactivating (or activating) gives them back as well
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    t.update_in(v, |t, window, cx| t.window_activation_changed(window, cx));
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert_prompt_has_the_keys(&t, v, "after the window changed activation");
}

#[gpui::test]
fn a_session_or_tab_switch_gives_the_keys_back(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    t.update(v, |t, cx| {
        t.live.as_mut().unwrap().live.sid = "another-session".into();
        cx.notify();
    });
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert_prompt_has_the_keys(&t, v, "after a session switch");
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    t.update(v, |t, cx| t.select_tab(super::ChatTab::Diff, cx));
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert_prompt_has_the_keys(&t, v, "after a tab switch");
}

#[gpui::test]
fn a_different_card_drops_the_keys_but_a_new_revision_of_the_same_one_keeps_them(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a") && focus_of(&t, v));
    let bump = |t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, id: Option<&'static str>| {
        t.update(v, |t, cx| {
            t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
                if let Some(d) = o.state.detail.as_mut()
                    && let Ok(cards) = d.cards.as_mut()
                {
                    match id {
                        Some(id) => cards[0].id = id.into(),
                        None => cards[0].revision += 1,
                    }
                }
                cx.notify();
            })
        });
        v.run_until_parked();
    };
    bump(&t, v, None);
    assert!(focus_of(&t, v), "same card, new revision: keeps the keys");
    bump(&t, v, Some("another_card"));
    assert!(shown(&t, v));
    assert_prompt_has_the_keys(&t, v, "after a different card arrived");
}

// ------------------------------------------------------------------------------ collapsed and expanded popup

/// The sample popup open by itself, with the keys, collapsed or expanded.
fn open_popup(
    cx: &mut TestAppContext,
    size: (f32, f32),
    expanded: bool,
) -> (gpui::Entity<TuiView>, &mut VisualTestContext) {
    let (t, v) = open(cx, size, Some(OutShot::Decision), Hidden::default());
    if expanded {
        t.update_in(v, |t, window, cx| {
            t.focus_decision(window, cx);
            t.expand_decision(cx);
        });
        v.run_until_parked();
    }
    (t, v)
}

fn field_count(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> usize {
    t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.card.rows.len(),
            _ => 0,
        }
    })
}

#[gpui::test]
fn collapsed_shows_the_first_field_and_says_how_many_are_not_shown(cx: &mut TestAppContext) {
    for size in [BIG, SMALL] {
        let (t, v) = open_popup(cx, size, false);
        let n = field_count(&t, v);
        assert!(n > 1, "the sample has several sections");
        assert!(v.debug_bounds("decision-field-0").is_some());
        assert!(
            v.debug_bounds("decision-field-1").is_none(),
            "only the first field when collapsed"
        );
        assert!(
            v.debug_bounds("decision-more").is_some(),
            "the hidden sections are announced next to the answers"
        );
        // the answers and buttons are all inside the card
        let d = b(v, "decision");
        for sel in [
            "decision-opt-0",
            "decision-opt-4",
            "decision-send",
            "decision-later",
            "decision-hint",
        ] {
            assert!(b(v, sel).bottom() <= d.bottom(), "{sel} is inside");
        }
    }
}

#[gpui::test]
fn expanding_shows_every_field_in_order_scrolling_inside_with_the_answers_pinned(
    cx: &mut TestAppContext,
) {
    for size in [BIG, SMALL] {
        let (t, v) = open_popup(cx, size, true);
        let n = field_count(&t, v);
        let (d, brief, prompt) = (b(v, "decision"), b(v, "decision-brief"), b(v, "prompt"));
        // every field exists in the brief box (laid out, scrolled inside it), in the server's order
        let mut last_y = f32::MIN;
        for i in 0..n {
            let sel: &'static str = Box::leak(format!("decision-field-{i}").into_boxed_str());
            let f = v.debug_bounds(sel);
            assert!(f.is_some(), "field {i} of {n} is in the brief");
            let y = f32::from(f.unwrap().origin.y);
            assert!(y >= last_y, "server order at {i}");
            last_y = y;
        }
        // the box is inside the card, the card above the prompt and under the tab strip, and the answers stay put
        assert!(brief.bottom() <= d.bottom());
        assert!(d.bottom() <= prompt.origin.y, "never over the prompt");
        assert!(
            d.origin.y >= b(v, "tabs").bottom(),
            "never over the tab strip"
        );
        assert!(b(v, "decision-opt-4").bottom() <= d.bottom());
        assert!(b(v, "decision-send").bottom() <= d.bottom());
        // Tab (the popup has the keys) collapses it again
        assert!(press(&t, v, "tab"));
        assert!(v.debug_bounds("decision-field-1").is_none());
    }
}

#[gpui::test]
fn the_popup_height_is_capped_to_a_share_of_the_chat_column(cx: &mut TestAppContext) {
    let (_, v) = open_popup(cx, BIG, false);
    let (d, chat) = (
        f32::from(b(v, "decision").size.height),
        f32::from(b(v, "p-chat").size.height),
    );
    assert!(d <= chat * 0.42 + 2., "collapsed: {d} of {chat}");
    assert!(d >= chat * 0.25, "and not tiny: {d} of {chat}");
    let (_, v) = open_popup(cx, BIG, true);
    let d = f32::from(b(v, "decision").size.height);
    assert!(d <= chat * 0.6 + 2., "expanded: {d} of {chat}");
    assert!(d > chat * 0.42, "expanded is taller than collapsed: {d}");
}

#[gpui::test]
fn a_new_revision_collapses_the_brief_again(cx: &mut TestAppContext) {
    let (t, v) = open_popup(cx, BIG, true);
    assert!(v.debug_bounds("decision-field-1").is_some());
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    assert!(
        v.debug_bounds("decision-field-1").is_none(),
        "collapsed again"
    );
}

// ------------------------------------------------------------------ fail closed on hidden sections

fn notice_of(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> Option<String> {
    t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.ask.notice.clone(),
            _ => None,
        }
    })
}

#[gpui::test]
fn send_needs_the_whole_brief_shown_once_per_card_and_revision(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    assert!(press(&t, v, "alt-a"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "2"));
    // collapsed with sections hidden: Enter sends nothing and says why
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0);
    assert_eq!(
        notice_of(&t, v).as_deref(),
        Some("Expand the brief first (Tab): 7 sections not shown")
    );
    // the button is the same press: inert too, and dim with the reason in its label
    let send = b(v, "decision-send").center();
    v.simulate_click(send, Default::default());
    assert_eq!(answers(&t, v), 0, "the Send button is inert too");
    // Tab expands: the choice stays; after the window, Enter sends once
    assert!(press(&t, v, "tab"));
    assert_eq!(popup_cursor(&t, v), Some(Some(1)));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 1, "sent once after the brief was shown");
    // a new revision: collapsed again, expand required again
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "2"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(
        answers(&t, v),
        1,
        "the new revision needs the brief shown again"
    );
    assert!(
        notice_of(&t, v)
            .unwrap()
            .starts_with("Expand the brief first")
    );
    // a card with nothing hidden sends without expanding (one section only)
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].rows.truncate(1);
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "2"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 2, "nothing hidden: no expand needed");
}

#[gpui::test]
fn a_single_long_field_is_cut_so_it_needs_the_expand_before_send(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    // one section only, but longer than the collapsed view shows
    t.update(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.update(cx, |o, cx| {
            if let Some(d) = o.state.detail.as_mut()
                && let Ok(cards) = d.cards.as_mut()
            {
                cards[0].rows.truncate(1);
                cards[0].rows[0].text = "long ".repeat(60);
                cards[0].revision += 1;
            }
            cx.notify();
        })
    });
    v.run_until_parked();
    assert!(press(&t, v, "alt-a"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "2"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0, "the cut field blocks Enter");
    assert_eq!(
        notice_of(&t, v).as_deref(),
        Some("Expand the brief first (Tab): text is cut")
    );
    let send = b(v, "decision-send").center();
    v.simulate_click(send, Default::default());
    assert_eq!(answers(&t, v), 0, "and the Send button");
    assert!(press(&t, v, "tab"));
    tick(&t, v, ARM_MS + 100);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 1, "sent after the field was shown whole");
}
