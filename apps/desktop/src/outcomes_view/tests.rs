use gpui::{Entity, TestAppContext, VisualTestContext, px, size};
use serde_json::{Value, json};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::outcomes::{DecisionCard, InspectorTab, WSL2_REFUSAL, uncovered};

use super::form;
use super::link;
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
    shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
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
        o.read_with(v, |o, _| o.state.ask.picker.as_ref().unwrap().cursor()),
        None
    );
    assert!(v.debug_bounds("out-opt-0").is_some() && v.debug_bounds("out-opt-4").is_some());
    // a click on the second answer moves the cursor and nothing else
    let b = v.debug_bounds("out-opt-1").unwrap();
    v.simulate_click(b.center(), Default::default());
    assert_eq!(
        o.read_with(v, |o, _| (
            o.state
                .ask
                .picker
                .as_ref()
                .unwrap()
                .choice()
                .map(String::from),
            o.state.ask.sending
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
            .ask
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
            .ask
            .picker
            .as_ref()
            .unwrap()
            .choice()
            .map(String::from)),
        Some("approve_with_w5".into())
    );
    v.simulate_keystrokes("enter");
    assert!(o.read_with(v, |o, _| {
        o.state.ask.notice.clone().unwrap().contains("Sample data")
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
    assert!(off.contains("synt.orchestrator.enabled") && off.contains("stub planner"));
    assert!(
        off.contains("nothing is started or migrated until you confirm"),
        "two explicit confirms, no implicit step"
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
    shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
    v.run_until_parked();
    let o = view(&shell, v);
    assert_eq!(o.read_with(v, |o, _| o.state.screen.clone()), Screen::Ready);
}

fn confirm_focused(o: &Entity<OutcomesView>, v: &mut VisualTestContext) -> bool {
    let h = o.read_with(v, |o, _| o.form_ui.as_ref().map(|u| u.confirm.clone()));
    h.is_some_and(|h| v.update(|window, _| h.is_focused(window)))
}

#[gpui::test]
fn the_new_outcome_form_is_keyboard_first_keeps_what_was_typed_and_sends_only_from_its_button(
    cx: &mut TestAppContext,
) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    v.simulate_keystrokes("n");
    v.run_until_parked();
    // the form is there with its base commit read (the sample answers locally), the goal field has the keys
    assert!(v.debug_bounds("out-form").is_some() && v.debug_bounds("out-form-confirm").is_some());
    let base = o.read_with(v, |o, _| o.state.form.as_ref().unwrap().head.clone());
    assert!(matches!(base, Some(form::Head::Known { .. })), "{base:?}");
    v.simulate_input("Move login to OIDC");
    // Enter inside a field is a new line, never a send
    v.simulate_keystrokes("enter");
    assert!(o.read_with(v, |o, _| o.state.form.as_ref().unwrap().errors.is_empty()));
    // Tab goes to the next field (not a tab character) and Shift+Tab back
    v.simulate_keystrokes("tab");
    v.simulate_input("Login works");
    v.simulate_keystrokes("shift-tab");
    v.simulate_input("!");
    let t = o.read_with(v, |o, cx| o.form_text(cx));
    assert_eq!(t.goal, "Move login to OIDC\n!");
    assert_eq!(t.criteria, "Login works");
    // Tab reaches the confirm button; with fields missing it shows every error inline and keeps the text
    for _ in 0..12 {
        if confirm_focused(&o, v) {
            break;
        }
        v.simulate_keystrokes("tab");
    }
    assert!(
        confirm_focused(&o, v),
        "the confirm button is on the Tab ring"
    );
    v.simulate_keystrokes("enter");
    v.run_until_parked();
    let errs = o.read_with(v, |o, _| o.state.form.as_ref().unwrap().errors.clone());
    assert!(errs.iter().any(|e| e.starts_with("Checks")), "{errs:?}");
    assert!(v.debug_bounds("out-form-errors").is_some());
    assert_eq!(
        o.read_with(v, |o, cx| o.form_text(cx)).criteria,
        "Login works"
    );
    // Esc closes without sending anything and gives the keys back to the tab
    v.simulate_keystrokes("escape");
    v.run_until_parked();
    assert!(o.read_with(v, |o, _| o.state.form.is_none() && o.form_ui.is_none()));
    assert!(v.debug_bounds("out-form").is_none());
    v.simulate_keystrokes("tab");
    assert_eq!(o.read_with(v, |o, _| o.state.pane), Pane::Centre);
}

#[gpui::test]
fn a_complete_new_outcome_form_builds_a_contract_and_the_sample_sends_nothing(
    cx: &mut TestAppContext,
) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    v.simulate_keystrokes("n");
    v.run_until_parked();
    // goal, criteria, checks, budget, scope
    for text in ["Move login", "Login works", "cargo test", "5", "auth/**"] {
        v.simulate_input(text);
        v.simulate_keystrokes("tab");
    }
    for _ in 0..12 {
        if confirm_focused(&o, v) {
            break;
        }
        v.simulate_keystrokes("tab");
    }
    v.simulate_keystrokes("enter");
    v.run_until_parked();
    // the sample refuses to send, says so inline and keeps the form and its text for another try
    let (errs, stage) = o.read_with(v, |o, _| {
        let f = o.state.form.as_ref().unwrap();
        (f.errors.clone(), f.stage.clone())
    });
    assert_eq!(errs, ["Sample data: nothing was sent."]);
    assert_eq!(stage, form::Stage::Edit);
    assert_eq!(o.read_with(v, |o, cx| o.form_text(cx)).budget, "5");
}

#[gpui::test]
fn the_review_shows_the_servers_warnings_and_approves_only_from_its_button(
    cx: &mut TestAppContext,
) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    v.simulate_keystrokes("v");
    v.run_until_parked();
    // the revise form starts from the current contract
    assert_eq!(
        o.read_with(v, |o, cx| o.form_text(cx)).scope,
        "auth/**\ntests/**"
    );
    assert!(
        v.debug_bounds("out-form-base").is_none(),
        "no base commit when revising"
    );
    o.update(v, |o, cx| {
        o.state
            .apply_form(link::FormMsg::Review(Box::new(sample::review(true))));
        cx.notify();
    });
    v.run_until_parked();
    // the widening and the lowered bar are shown on their own, as the server worded them
    assert!(v.debug_bounds("out-widens").is_some() && v.debug_bounds("out-weakens").is_some());
    // nothing is selected for the person: Enter with no button focused approves nothing
    v.simulate_keystrokes("enter");
    assert!(o.read_with(v, |o, _| matches!(
        o.state.form.as_ref().unwrap().stage,
        form::Stage::Review(_)
    )));
    for _ in 0..4 {
        if confirm_focused(&o, v) {
            break;
        }
        v.simulate_keystrokes("tab");
    }
    assert!(confirm_focused(&o, v));
    v.simulate_keystrokes("enter");
    v.run_until_parked();
    // the sample sends nothing: the review is still there, with the refusal inline
    let (errs, review) = o.read_with(v, |o, _| {
        (
            o.state.form.as_ref().unwrap().errors.clone(),
            o.state.form_review().is_some(),
        )
    });
    assert!(
        review && errs == ["Sample data: nothing was sent."],
        "{errs:?}"
    );
}

#[gpui::test]
fn the_form_fits_both_window_sizes(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    for (w, h) in [(1400., 900.), (1100., 700.)] {
        let (shell, v) = open(cx, w, h);
        let o = view(&shell, v);
        v.simulate_keystrokes("n");
        v.run_until_parked();
        for sel in ["out-form", "out-form-confirm", "out-form-cancel"] {
            let b = bounds(v, sel);
            assert!(
                b.0 >= 0. && b.1 >= 0. && b.0 + b.2 <= w && b.1 + b.3 <= h,
                "{w}x{h} {sel}: {b:?}"
            );
        }
        let _ = o;
    }
}

#[gpui::test]
fn a_held_enter_or_one_in_the_frame_the_review_appears_approves_nothing(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    v.simulate_keystrokes("v");
    v.run_until_parked();
    o.update(v, |o, cx| {
        o.state
            .apply_form(link::FormMsg::Review(Box::new(sample::review(true))));
        // confirm pressed before the review was ever drawn
        o.confirm(cx);
    });
    assert!(o.read_with(v, |o, _| o.state.form_review().is_some()
        && matches!(o.state.form.as_ref().unwrap().stage, form::Stage::Review(_))));
    v.run_until_parked();
    for _ in 0..4 {
        if confirm_focused(&o, v) {
            break;
        }
        v.simulate_keystrokes("tab");
    }
    assert!(confirm_focused(&o, v));
    // a held (auto-repeated) Enter on the focused button does nothing
    let held = gpui::KeyDownEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    v.simulate_event(held);
    assert!(o.read_with(v, |o, _| matches!(
        o.state.form.as_ref().unwrap().stage,
        form::Stage::Review(_)
    )));
    // one deliberate press does (the sample answers that nothing was sent)
    v.simulate_keystrokes("enter");
    let errs = o.read_with(v, |o, _| o.state.form.as_ref().unwrap().errors.clone());
    assert_eq!(errs, ["Sample data: nothing was sent."]);
}

// ------------------------------------------------------------------------ the guided enable flow

fn guide_view(
    cx: &mut TestAppContext,
    screen: Screen,
    w: f32,
    h: f32,
) -> (Entity<OutcomesView>, &mut VisualTestContext) {
    let (shell, v) = open(cx, w, h);
    let o = view(&shell, v);
    o.update(v, |o, cx| {
        o.state.screen = screen;
        cx.notify();
    });
    v.run_until_parked();
    (o, v)
}

#[gpui::test]
fn the_off_screen_has_a_button_that_only_opens_the_disclosure_verbatim(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    for (w, h) in [(1400., 900.), (1100., 700.)] {
        let (o, v) = guide_view(cx, Screen::Off, w, h);
        assert!(
            v.debug_bounds("out-guide").is_none() && v.debug_bounds("out-guide-open").is_some()
        );
        let b = v.debug_bounds("out-guide-open").unwrap();
        v.simulate_click(b.center(), Default::default());
        v.run_until_parked();
        let g = o.read_with(v, |o, _| o.state.guide.clone().unwrap());
        assert_eq!(
            (g.step, g.focus, g.busy),
            (model::Step::Setting, None, false)
        );
        let (p, t) = (bounds(v, "out-guide"), bounds(v, "out-guide-text"));
        assert!(
            p.0 >= 0. && p.0 + p.2 <= w && p.1 + p.3 <= h,
            "{w}x{h}: {p:?}"
        );
        assert!(t.3 > 40., "the notice has room: {t:?}");
        for sel in ["out-guide-confirm", "out-guide-cancel"] {
            let b = bounds(v, sel);
            assert!(b.0 + b.2 <= w && b.1 + b.3 <= h, "{sel} on screen: {b:?}");
        }
        // the sample has no settings file: confirming writes nothing and says so
        let c = v.debug_bounds("out-guide-confirm").unwrap();
        v.simulate_click(c.center(), Default::default());
        let g = o.read_with(v, |o, _| o.state.guide.clone().unwrap());
        assert!(!g.busy && g.error.as_deref().unwrap().contains("Sample data"));
    }
}

#[gpui::test]
fn the_keyboard_needs_tab_then_enter_to_confirm_and_esc_cancels(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (o, v) = guide_view(cx, Screen::NotReady("not migrated".into()), 1100., 700.);
    let guide = |o: &Entity<OutcomesView>, v: &VisualTestContext| {
        o.read_with(v, |o, _| o.state.guide.clone())
    };
    v.simulate_keystrokes("enter");
    assert_eq!(guide(&o, v).unwrap().step, model::Step::Migrate);
    // Enter with nothing focused does nothing; the first Tab lands on Cancel, never on Confirm
    v.simulate_keystrokes("enter");
    assert_eq!(guide(&o, v).map(|g| (g.focus, g.busy)), Some((None, false)));
    v.simulate_keystrokes("tab");
    assert_eq!(guide(&o, v).unwrap().focus, Some(model::Btn::Cancel));
    v.simulate_keystrokes("escape");
    assert!(guide(&o, v).is_none(), "Esc cancels");
    // again: Tab, Tab, Enter reaches Confirm (the sample answers locally: nothing is sent)
    v.simulate_keystrokes("enter tab tab");
    assert_eq!(guide(&o, v).unwrap().focus, Some(model::Btn::Confirm));
    v.simulate_keystrokes("enter");
    assert!(guide(&o, v).unwrap().error.unwrap().contains("Sample data"));
}

#[gpui::test]
fn the_notice_shown_is_the_builds_own_text_line_for_line(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (o, v) = guide_view(cx, Screen::Off, 1400., 900.);
    o.update(v, |o, cx| {
        o.state.open_guide();
        cx.notify();
    });
    v.run_until_parked();
    // the dialog draws `ENABLE_NOTICE.lines()` and nothing else as its body
    assert!(v.debug_bounds("out-guide-text").is_some());
    let n = synthesis_app::outcome_cmd::ENABLE_NOTICE;
    assert!(n.starts_with(synthesis_app::outcome_policy::INTEGRITY_DISCLOSURE));
    assert!(n.contains("schema v5") && n.contains("backup"));
}

#[gpui::test]
fn a_closed_gate_shows_the_refusal_in_the_dialog_instead_of_the_confirm(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    for screen in [Screen::Off, Screen::NotReady("not migrated".into())] {
        let (o, v) = guide_view(cx, screen, 1100., 700.);
        let why = "the release gate is not passed";
        o.update(v, |o, _| o.state.gate_refusal = Some(why.into()));
        let b = v.debug_bounds("out-guide-open").unwrap();
        v.simulate_click(b.center(), Default::default());
        v.run_until_parked();
        assert!(v.debug_bounds("out-guide").is_some());
        assert!(v.debug_bounds("out-guide-confirm").is_none(), "no confirm");
        assert!(v.debug_bounds("out-guide-cancel").is_some());
        let g = o.read_with(v, |o, _| o.state.guide.clone().unwrap());
        assert_eq!(g.error.as_deref(), Some(why));
        // Tab, Tab, Enter cannot confirm either: nothing is sent
        v.simulate_keystrokes("tab tab enter");
        assert!(o.update(v, |o, _| o.state.confirm_guide()).is_empty());
        v.simulate_keystrokes("escape");
        assert!(o.read_with(v, |o, _| o.state.guide.is_none()));
    }
}

#[test]
fn the_picker_lists_the_default_first_then_known_folders_once_and_a_pick_resets_the_base_commit() {
    let mut st = sample::state();
    st.workspace = "/w/app".into();
    st.known = vec!["/w/lib".into(), "/w/app".into(), "/x/other".into()];
    // the default is preselected in the form and first in the list, no repeats; "Choose folder..." is one more item
    assert_eq!(st.ws_items(), ["/w/app", "/w/lib", "/x/other"]);
    assert_eq!(st.ws_len(), 4);
    st.open_new();
    assert_eq!(st.form.as_ref().unwrap().initial.workspace, "/w/app");
    st.form
        .as_mut()
        .unwrap()
        .head_known("/w/app", Ok("abc".into()));
    st.ws_menu = Some(1);
    // a folder from the list or the dialog: the list closes, the stale commit is replaced by a read of the new folder
    let e = st.pick_workspace("/w/lib");
    assert_eq!(st.ws_menu, None);
    assert!(
        matches!(e.as_slice(), [Effect::Head(w)] if w == "/w/lib"),
        "{e:?}"
    );
    assert!(matches!(
        st.form.as_ref().unwrap().head,
        Some(form::Head::Resolving(_))
    ));
    // the same folder again reads nothing
    assert!(st.pick_workspace("/w/lib").is_empty());
}

#[test]
fn a_remote_engine_has_no_folder_dialog_item() {
    let mut st = sample::state();
    st.known = vec!["/srv/a".into()];
    st.remote = true;
    assert_eq!(st.ws_len(), st.ws_items().len());
}

#[test]
fn folder_names_split_and_cut_in_the_middle() {
    use super::form_view::{middle, split_folder};
    assert_eq!(
        split_folder("/Users/x/Vibe/app/"),
        ("app".into(), "/Users/x/Vibe".into())
    );
    assert_eq!(split_folder(r"C:\w\app"), ("app".into(), r"C:\w".into()));
    assert_eq!(middle("short", 10), "short");
    let m = middle("/aaaaaaaaaa/bbbbbbbbbb/cccccccccc", 11);
    assert_eq!(m.chars().count(), 11);
    assert!(m.starts_with("/aaaa") && m.ends_with("cccc") && m.contains('\u{2026}'));
}

#[gpui::test]
fn the_form_shows_a_read_only_picker_and_a_pick_fills_the_field(cx: &mut TestAppContext) {
    let _g = theme::opacity_guard();
    let (shell, v) = open(cx, 1400., 900.);
    let o = view(&shell, v);
    o.update(v, |o, _| o.state.known = vec!["/w/lib".into()]);
    v.simulate_keystrokes("n");
    v.run_until_parked();
    assert!(v.debug_bounds("out-form-ws").is_some());
    // local: no text field for the folder, the list opens from the button
    o.update(v, |o, cx| {
        o.state.ws_menu = Some(0);
        cx.notify();
    });
    v.run_until_parked();
    assert!(v.debug_bounds("out-form-ws-list").is_some());
    o.update_in(v, |o, window, cx| o.ws_choose(1, window, cx));
    v.run_until_parked();
    assert_eq!(o.read_with(v, |o, cx| o.form_text(cx)).workspace, "/w/lib");
    assert!(v.debug_bounds("out-form-ws-list").is_none());
}
