//! Geometry and key tests of the outcome parts of the TUI view (tab, Approvals panel, decision popup) on the
//! sample data, and of the guarantee that none of it exists while outcomes are off. The tests of `design_tests`
//! (measured against `cli.html`) run with no outcomes link, so they also prove the off state changes no geometry.

use gpui::{AppContext as _, Bounds, Pixels, TestAppContext, VisualTestContext, px, size};

use super::TuiView;
use super::approvals_ui::{OutShot, Shown};
use super::layout::Hidden;
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
    for i in 0..3 {
        assert!(
            v.debug_bounds(["appr-row-0", "appr-row-1", "appr-row-2"][i])
                .is_some()
        );
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

#[gpui::test]
fn the_decision_popup_sits_above_the_prompt_with_nothing_chosen(cx: &mut TestAppContext) {
    for size in [BIG, SMALL] {
        let (t, v) = open(cx, size, Some(OutShot::Decision), Hidden::default());
        let (d, prompt, chat) = (b(v, "decision"), b(v, "prompt"), b(v, "p-chat"));
        assert!(d.bottom() <= prompt.origin.y, "above the prompt");
        assert!(d.origin.y >= chat.origin.y && d.bottom() <= chat.bottom());
        for i in 0..5 {
            let sel = [
                "decision-opt-0",
                "decision-opt-1",
                "decision-opt-2",
                "decision-opt-3",
                "decision-opt-4",
            ][i];
            let o = b(v, sel);
            assert!(
                o.bottom() <= d.bottom(),
                "the answers stay in the card: {sel}"
            );
        }
        assert!(v.debug_bounds("decision-send").is_some());
        let cursor = t.read_with(v, |t, _| {
            match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
                Some(Shown::Card(p, _)) => p.ask.picker.as_ref().map(|p| p.cursor()),
                _ => panic!("no card"),
            }
        });
        assert_eq!(cursor, Some(None), "no preselection");
    }
}

#[gpui::test]
fn the_popup_is_answered_in_two_steps_and_esc_is_later(cx: &mut TestAppContext) {
    let (t, v) = open(cx, BIG, Some(OutShot::Decision), Hidden::default());
    let sending = |t: &gpui::Entity<TuiView>, v: &mut VisualTestContext| {
        t.read_with(v, |t, cx| {
            t.outcomes
                .as_ref()
                .unwrap()
                .view
                .read(cx)
                .state
                .ask
                .notice
                .clone()
        })
    };
    // Enter first: nothing chosen, nothing sent
    assert!(press(&t, v, "enter"));
    assert_eq!(sending(&t, v), None);
    // step one: an arrow chooses and sends nothing
    assert!(press(&t, v, "down"));
    assert_eq!(sending(&t, v), None);
    // step two inside the arming delay: still nothing (the fixed clock is the moment it appeared)
    assert!(press(&t, v, "enter"));
    assert_eq!(sending(&t, v), None);
    // Tab shows the whole brief
    assert!(press(&t, v, "tab"));
    assert!(t.read_with(v, |t, _| matches!(
        t.outcomes.as_ref().and_then(|w| w.shown.as_ref()),
        Some(Shown::Card(p, _)) if p.full
    )));
    // Esc: later, the popup goes and the row stays in Approvals
    assert!(press(&t, v, "escape"));
    assert!(t.read_with(v, |t, _| t.outcomes.as_ref().unwrap().shown.is_none()));
    assert!(v.debug_bounds("decision").is_none());
    assert!(v.debug_bounds("p-approvals").is_some());
}

#[gpui::test]
fn alt_a_goes_to_the_next_approval_and_alt_0_to_the_outcomes_tab(cx: &mut TestAppContext) {
    let (t, v) = open(cx, BIG, Some(OutShot::Approvals1), Hidden::default());
    // the one row is a decision on the selected outcome: its popup opens
    assert!(press(&t, v, "alt-a"));
    assert!(v.debug_bounds("decision").is_some());
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

// ------------------------------------------------------------------------------------- key safety

/// A live view (no engine) with the Decision sample open and the clock past the arming delay.
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
    t.update(v, |t, _| {
        t.outcomes.as_mut().unwrap().fixed_now = Some(super::sample::OUTCOMES_NOW + 10_000)
    });
    (t, v)
}

fn popup_cursor(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> Option<Option<usize>> {
    t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.ask.picker.as_ref().map(|p| p.cursor()),
            _ => None,
        }
    })
}

/// The clock of the view moves on by `ms`.
fn tick(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, ms: i64) {
    t.update(v, |t, _| {
        let w = t.outcomes.as_mut().unwrap();
        w.fixed_now = w.fixed_now.map(|n| n + ms);
    });
}

fn answers(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext) -> u64 {
    t.read_with(v, |t, cx| {
        t.outcomes.as_ref().unwrap().view.read(cx).state.answers
    })
}

fn set_prompt(t: &gpui::Entity<TuiView>, v: &mut VisualTestContext, s: &'static str) {
    t.update_in(v, |t, window, cx| t.set_prompt(s, window, cx));
    v.run_until_parked();
}

#[gpui::test]
fn typed_text_keeps_digits_enter_arrows_and_shift_chords_for_the_prompt(cx: &mut TestAppContext) {
    let (t, v) = open_live(cx);
    set_prompt(&t, v, "half a message");
    // with text in the prompt none of these is the popup's: not consumed here, nothing chosen, nothing sent
    for k in [
        "1",
        "2",
        "9",
        "enter",
        "shift-enter",
        "shift-2",
        "shift-3",
        "shift-1",
        "up",
        "down",
    ] {
        set_prompt(&t, v, "half a message");
        let consumed = press(&t, v, k);
        // up and down are the engine's (prompt history), the rest is plain text or a submit: never the popup's
        assert!(
            !consumed || matches!(k, "up" | "down"),
            "{k} must reach the prompt"
        );
        assert_eq!(popup_cursor(&t, v), Some(None), "{k} chose nothing");
        assert_eq!(answers(&t, v), 0, "{k} sent nothing");
    }
    // Shift+Enter and Shift+digit pass through even with an empty prompt, and even with a choice made
    set_prompt(&t, v, "");
    assert!(press(&t, v, "down"));
    assert_eq!(popup_cursor(&t, v), Some(Some(0)));
    for k in ["shift-enter", "shift-1", "shift-2"] {
        assert!(!press(&t, v, k), "{k}");
        assert_eq!(popup_cursor(&t, v), Some(Some(0)), "{k} moved nothing");
        assert_eq!(answers(&t, v), 0, "{k} sent nothing");
    }
}

#[gpui::test]
fn an_answer_in_flight_blocks_the_other_surface_and_a_reply_clears_the_choice(
    cx: &mut TestAppContext,
) {
    let (t, v) = open_live(cx);
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
    // now it sends (the sample answers at once with "nothing was sent", as a failed answer would); a later moment
    // than the blocked Enter, which a held key would not be
    tick(&t, v, 1000);
    assert!(press(&t, v, "enter"));
    v.run_until_parked();
    assert_eq!(answers(&t, v), 1);
    // the reply cleared the choice: one more Enter resends nothing
    assert_eq!(
        popup_cursor(&t, v),
        Some(None),
        "the choice is gone after the reply"
    );
    tick(&t, v, 1000);
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 1, "no resend after a reply");
}

#[gpui::test]
fn other_overlays_keep_their_keys_and_a_dialog_re_arms_the_popup(cx: &mut TestAppContext) {
    use crate::live::{Dialog, DialogKind};
    let (t, v) = open_live(cx);
    let shown = |t: &gpui::Entity<TuiView>, v: &mut VisualTestContext| {
        t.read_with(v, |t, _| t.outcomes.as_ref().unwrap().shown.is_some())
    };
    // the history search (Ctrl+R) owns Esc while it is open: the popup stays
    t.update(v, |t, _| {
        t.live.as_mut().unwrap().search = Some((String::new(), 0))
    });
    assert!(press(&t, v, "escape"), "the search took it");
    assert!(t.read_with(v, |t, _| t.live.as_ref().unwrap().search.is_none()));
    assert!(shown(&t, v), "Esc closed the search, not the popup");
    // a tool dialog owns the keyboard: the popup stays, its Enter is inert for ARM_MS from now
    t.update(v, |t, _| {
        t.live.as_mut().unwrap().live.dialogs.push(Dialog {
            kind: DialogKind::Plan {
                id: "p".into(),
                plan: "do it".into(),
            },
            options: vec![("yes".into(), "Yes".into()), ("no".into(), "No".into())],
            sel: 0,
            armed_until: 0,
            engaged: false,
        })
    });
    assert!(press(&t, v, "escape"), "the dialog rejected");
    assert!(t.read_with(v, |t, _| t.live.as_ref().unwrap().live.dialogs.is_empty()));
    assert!(shown(&t, v), "the dialog's Esc did not close the popup");
    let armed = t.read_with(v, |t, _| {
        match t.outcomes.as_ref().and_then(|w| w.shown.as_ref()) {
            Some(Shown::Card(p, _)) => p.ask.armed_until,
            _ => 0,
        }
    });
    assert!(armed >= super::sample::OUTCOMES_NOW + 10_000 + crate::live::ARM_MS);
    // so a choice and an Enter right after the dialog closed send nothing
    assert!(press(&t, v, "down"));
    assert!(press(&t, v, "enter"));
    assert_eq!(answers(&t, v), 0);
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
