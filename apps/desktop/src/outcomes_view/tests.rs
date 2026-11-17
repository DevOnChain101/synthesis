use gpui::{Entity, TestAppContext, VisualTestContext, px, size};
use serde_json::{Value, json};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::outcomes::{DecisionCard, InspectorTab, WSL2_REFUSAL, uncovered};

use super::model::{CentreTab, OFF_LINES, Pane, Screen};
use super::render::{card_layout, state_colour, state_rgb};
use super::*;
use crate::theme::{self, hsla};
use crate::ui::{Shell, View};

fn open(cx: &mut TestAppContext, w: f32, h: f32) -> (Entity<Shell>, &mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::apply("JetBrains Mono", cx);
    });
    let (shell, v) = cx.add_window_view(|window, cx| {
        Shell::new(window, cx, None, Default::default(), Err("test".into()))
    });
    v.simulate_resize(size(px(w), px(h)));
    shell.update_in(v, |s, window, cx| s.go(View::Outcomes, window, cx));
    v.run_until_parked();
    (shell, v)
}

fn view(shell: &Entity<Shell>, v: &VisualTestContext) -> Entity<OutcomesView> {
    shell.read_with(v, |s, _| s.outcomes().clone())
}

fn bounds(v: &mut VisualTestContext, sel: &'static str) -> (f32, f32, f32, f32) {
    let b = v.debug_bounds(sel).unwrap_or_else(|| panic!("no {sel}"));
    (
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

#[gpui::test]
fn three_columns_fit_the_window_without_overlap_at_both_sizes(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    for (w, h) in [(1400., 900.), (1100., 700.)] {
        let (_, v) = open(cx, w, h);
        let (l, c, r) = (
            bounds(v, "out-left"),
            bounds(v, "out-centre"),
            bounds(v, "out-right"),
        );
        assert!(
            l.0 + l.2 <= c.0 && c.0 + c.2 <= r.0 && r.0 + r.2 <= w,
            "{w}: {l:?} {c:?} {r:?}"
        );
        assert!(c.2 >= 460., "{w}: the decision card keeps room: {c:?}");
        // the columns end above the footer, which ends at the window's bottom
        let f = bounds(v, "footer");
        assert_eq!(f.1 + f.3, h);
        for col in [l, c, r] {
            assert!(col.1 + col.3 <= f.1, "{col:?} over the footer");
        }
        // the first decision's answers are on screen
        let send = bounds(v, "out-send");
        assert!(send.0 + send.2 <= w, "{w}x{h}: {send:?}");
    }
}

#[gpui::test]
fn the_decision_card_starts_with_no_answer_chosen_and_a_click_chooses_without_sending(
    cx: &mut TestAppContext,
) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    assert_eq!(
        o.read_with(v, |o, _| o.state.picker.as_ref().unwrap().cursor()),
        None
    );
    assert!(v.debug_bounds("out-opt-0").is_some() && v.debug_bounds("out-opt-4").is_some());
    // a click on the second answer moves the cursor and nothing else
    let b = v.debug_bounds("out-opt-1").unwrap();
    v.simulate_click(b.center(), Default::default());
    assert_eq!(
        o.read_with(v, |o, _| (
            o.state.picker.as_ref().unwrap().choice().map(String::from),
            o.state.sending
        )),
        (Some("approve_without_c3".into()), false)
    );
    // Send is the second step; in the sample it sends nothing and says so
    // the card is longer than the column: scroll to its end (the cue under it says so), where Send is
    o.update(v, |o, cx| {
        let max = o.scroll[1].max_offset().y;
        o.scroll[1].set_offset(gpui::point(px(0.), -max));
        cx.notify();
    });
    v.run_until_parked();
    let b = v.debug_bounds("out-send").unwrap();
    v.simulate_click(b.center(), Default::default());
    assert!(o.read_with(v, |o, _| {
        o.state
            .notice
            .clone()
            .unwrap()
            .contains("Sample data: nothing was sent")
    }));
}

#[gpui::test]
fn the_keyboard_alone_reaches_every_column_and_answers_in_two_steps(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    // tab to the centre, down chooses the first answer, enter sends (the sample: nothing leaves the app)
    v.simulate_keystrokes("tab");
    assert_eq!(o.read_with(v, |o, _| o.state.pane), Pane::Centre);
    v.simulate_keystrokes("down");
    assert_eq!(
        o.read_with(v, |o, _| o
            .state
            .picker
            .as_ref()
            .unwrap()
            .choice()
            .map(String::from)),
        Some("approve_with_w5".into())
    );
    v.simulate_keystrokes("enter");
    assert!(o.read_with(v, |o, _| {
        o.state.notice.clone().unwrap().contains("Sample data")
    }));
    v.simulate_keystrokes("tab");
    assert_eq!(o.read_with(v, |o, _| o.state.pane), Pane::Inspector);
    v.simulate_keystrokes("4");
    assert_eq!(o.read_with(v, |o, _| o.state.itab), InspectorTab::Diff);
    v.run_until_parked();
    assert!(v.debug_bounds("out-right").is_some());
    v.simulate_keystrokes("shift-tab");
    assert_eq!(o.read_with(v, |o, _| o.state.pane), Pane::Centre);
}

#[gpui::test]
fn every_inspector_tab_and_centre_tab_draws_inside_its_column(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1100., 700.);
    let o = view(&shell, v);
    for t in InspectorTab::ALL {
        for c in CentreTab::ALL {
            o.update(v, |o, cx| {
                let item = o.state.items[0].clone();
                o.state.itab = t;
                o.state.inspector = Some(sample::inspector(t, &item));
                o.state.centre = c;
                cx.notify();
            });
            v.run_until_parked();
            let r = bounds(v, "out-right");
            let ce = bounds(v, "out-centre");
            assert!(r.0 + r.2 <= 1100. && ce.0 + ce.2 <= r.0, "{t:?} {c:?}");
        }
    }
}

#[gpui::test]
fn the_screens_that_are_not_the_list_explain_themselves(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1100., 700.);
    let o = view(&shell, v);
    for s in [
        Screen::Off,
        Screen::Windows,
        Screen::NotReady("run synt outcome enable".into()),
        Screen::Unreachable("the orchestrator is not running".into()),
        Screen::Loading,
    ] {
        o.update(v, |o, cx| {
            o.state.screen = s.clone();
            cx.notify();
        });
        v.run_until_parked();
        let m = bounds(v, "out-message");
        assert!(
            m.0 >= 0. && m.0 + m.2 <= 1100. && m.1 + m.3 <= 700.,
            "{s:?} {m:?}"
        );
        assert!(
            v.debug_bounds("out-left").is_none(),
            "no list behind the {s:?} screen"
        );
    }
    // the words the screens carry
    let off = OFF_LINES.join(" ");
    assert!(off.contains("synt.orchestrator.enabled") && off.contains("synt outcome enable"));
    assert!(
        off.contains("does not turn outcomes on"),
        "no enabling from the window"
    );
    assert!(WSL2_REFUSAL.contains("WSL2"));
}

fn briefs() -> Vec<Value> {
    vec![
        json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
               "alternatives": [{"id": "REST", "label": "REST", "tradeoffs": "simple, chatty"},
                                {"id": "RPC", "label": "RPC", "tradeoffs": "fast, coupled"}],
               "evidence": {"work_item": "one", "attempt_ordinal": 3, "refused_calls": ["Bash: rm x"]},
               "unknowns": ["is the client typed?"], "defer_consequence": "the item stays on hold",
               "safe_default": "HOLD", "future_field": "shown even though this build does not know it"}),
        // a plan decision: nested fields of every item, a diff, a trigger
        json!({"trigger": "item_blocked", "plan": {"source": "model", "plan_hash": "abc123", "items": [
                {"key": "w1", "kind": "code", "requires": [], "title": "first", "instructions": "do the first thing"},
                {"key": "w2", "kind": "test", "requires": ["w1"], "title": "second", "instructions": "x\ny"}],
               "max_attempts_per_work_item": 3, "outcome_cost_cap_usd": 5.5,
               "diff_vs_current_plan": {"current_plan_version": 2, "diff": {"added": ["w2"], "removed": [], "changed": ["w1"]}}},
               "alternatives": [{"id": "approve_model_plan", "label": "Approve"}, {"id": "keep_current_plan"}]}),
        json!({"unknown_a": {"nested": {"deep": "kept"}}, "alternatives": [], "why_human": "e\u{1b}[31mscaped"}),
    ]
}

#[test]
fn the_card_layout_shows_every_non_empty_field_of_the_brief_and_the_proof_of_what_was_seen() {
    for brief in briefs() {
        let c = DecisionCard::from_json(&json!({"id": "d1", "kind": "ambiguity", "state": "pending", "revision": 7,
            "brief_sha256": "c".repeat(64), "expires_at": "2030-01-01T00:00:00.000Z", "brief": brief.clone()}))
        .unwrap();
        let lay = card_layout(&c);
        let shown = lay.text();
        assert_eq!(uncovered(&brief, &shown), Vec::<String>::new(), "{shown}");
        // the server's state, expiry, the revision and the whole brief hash are on the card
        for want in [
            "pending",
            "2030-01-01T00:00:00.000Z",
            "revision 7",
            &"c".repeat(64),
        ] {
            assert!(shown.contains(want), "{want} in {shown}");
        }
        assert!(!shown.contains('\u{1b}'), "control text made visible");
        assert!(!lay.rows.is_empty());
    }
    // the check does catch a layout that drops a field
    let c = DecisionCard::from_json(
        &json!({"id": "d1", "kind": "k", "state": "pending", "revision": 1,
        "brief_sha256": "d".repeat(64), "brief": briefs().remove(0)}),
    )
    .unwrap();
    assert!(
        !uncovered(
            &briefs()[0],
            &card_layout(&c).text().replace("fast, coupled", "")
        )
        .is_empty()
    );
}

#[test]
fn state_colours_stay_in_the_platinum_palette_and_readable() {
    let lum = |c: t::Rgb| {
        let l = |v: u8| {
            let v = f64::from(v) / 255.;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * l(c[0]) + 0.7152 * l(c[1]) + 0.0722 * l(c[2])
    };
    let ratio = |a: t::Rgb, b: t::Rgb| {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    };
    let words = [
        "verified",
        "blocked",
        "pending",
        "running",
        "created",
        "unknown",
        "waiting_decision",
        "lost",
        "approved",
    ];
    for w in words {
        let rgb = state_rgb(w);
        assert!(
            !(rgb[2] > rgb[1] && rgb[2] > rgb[0].saturating_add(15)),
            "{w}: blue or violet {rgb:?}"
        );
        for ground in [t::BG, t::SURFACE, t::ELEVATED] {
            assert!(ratio(rgb, ground) >= 4.5, "{w} {rgb:?} on {ground:?}");
        }
    }
    // the ink of the Send button on its accent, the dim text on the ground of the cards
    assert!(ratio(t::ACCENT_INK, t::ACCENT) >= 4.5);
    assert!(ratio(crate::widgets::chrome::DIM, t::ELEVATED) >= 4.5);
    assert_eq!(state_colour("pending"), hsla(t::WARN));
}

#[gpui::test]
fn settings_over_the_outcomes_tab_and_back_keeps_the_tab(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1100., 700.);
    shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
    v.run_until_parked();
    assert!(
        v.debug_bounds("outcomes").is_some(),
        "the tab is under the panel"
    );
    shell.update_in(v, |s, window, cx| s.go(View::Outcomes, window, cx));
    v.run_until_parked();
    let o = view(&shell, v);
    assert_eq!(o.read_with(v, |o, _| o.state.screen.clone()), Screen::Ready);
}
