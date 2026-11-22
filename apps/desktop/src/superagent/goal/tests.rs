use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};
use synthesis_core::outcome::ContractKind;

use super::model::{Field, approve_ok, field_of, why_not_send};
use super::*;
use crate::outcomes_view::form::Submit;
use crate::outcomes_view::sample;

const COMMIT: &str = "9433c678e4b1d2f0a7c35e8b61d9f4a02c7b5e13";

fn dir() -> String {
    std::env::temp_dir().display().to_string()
}

type Seen = Rc<RefCell<Vec<String>>>;

fn open(
    cx: &mut TestAppContext,
    kind: GoalKind,
) -> (Entity<GoalForm>, &mut VisualTestContext, Seen) {
    cx.update(gpui_component::init);
    let seen: Seen = Rc::default();
    let s2 = seen.clone();
    let (f, v) = cx.add_window_view(move |window, cx| {
        let g = GoalForm::new(kind, None, &dir(), window, cx);
        let s3 = s2.clone();
        cx.subscribe(&cx.entity(), move |_, _, e: &GoalEvent, _| {
            s3.borrow_mut().push(match e {
                GoalEvent::Close => "close".into(),
                GoalEvent::Approved { outcome, .. } => format!("approved {outcome}"),
            })
        })
        .detach();
        g
    });
    v.simulate_resize(size(px(1000.), px(1000.)));
    v.run_until_parked();
    (f, v, seen)
}

fn type_in(v: &mut VisualTestContext, f: &Entity<GoalForm>, field: Field, text: &str) {
    f.update_in(v, |f, w, cx| f.set_text(field, text, w, cx));
    v.run_until_parked();
}

fn key(v: &mut VisualTestContext, f: &Entity<GoalForm>, k: &str, m: Modifiers, held: bool) -> bool {
    let ev = KeyDownEvent {
        keystroke: Keystroke {
            modifiers: m,
            key: k.into(),
            key_char: None,
        },
        is_held: held,
        prefer_character_input: false,
    };
    let r = f.update_in(v, |f, w, cx| f.on_key(&ev, w, cx));
    v.run_until_parked();
    r
}

fn none() -> Modifiers {
    Modifiers::default()
}

fn cmd(v: &mut VisualTestContext) -> Modifiers {
    v.update(|_, cx| Modifiers::secondary_key(cx))
}

fn head_known(v: &mut VisualTestContext, f: &Entity<GoalForm>) {
    f.update(v, |f, cx| {
        f.apply_form(
            FormMsg::Head {
                ws: dir(),
                result: Ok(COMMIT.into()),
            },
            cx,
        )
    });
}

fn fill_writes(v: &mut VisualTestContext, f: &Entity<GoalForm>) {
    for (k, t) in [
        (Field::Goal, "Move login to OIDC"),
        (Field::Criteria, "Login works with the test IdP"),
        (Field::Checks, "cargo test -p auth"),
        (Field::Scope, "auth/**"),
        (Field::Budget, "5"),
    ] {
        type_in(v, f, k, t);
    }
}

fn fill_read_only(v: &mut VisualTestContext, f: &Entity<GoalForm>) {
    type_in(v, f, Field::Goal, "Which modules use the old logger?");
    type_in(v, f, Field::Criteria, "Names every module");
    type_in(v, f, Field::Budget, "2");
    type_in(v, f, Field::Folder, &dir());
}

fn creates(f: &Entity<GoalForm>, v: &mut VisualTestContext) -> Vec<Out> {
    f.read_with(v, |f, _| f.outbox.clone())
        .into_iter()
        .filter(|o| !matches!(o, Out::Head(_)))
        .collect()
}

fn review(f: &Entity<GoalForm>, v: &mut VisualTestContext) {
    f.update(v, |f, cx| {
        f.apply_form(FormMsg::Review(Box::new(sample::review(false))), cx)
    });
    v.run_until_parked();
}

#[gpui::test]
fn an_empty_form_sends_nothing_whatever_is_pressed(cx: &mut TestAppContext) {
    let (f, v, seen) = open(cx, GoalKind::Writes);
    assert!(v.debug_bounds("sa-goal-goal").is_some());
    assert_eq!(
        f.read_with(v, |f, cx| f.why_not_send(cx)),
        Some("Write the goal first")
    );
    let c = cmd(v);
    key(v, &f, "enter", c, false);
    key(v, &f, "enter", none(), false);
    f.update(v, |f, cx| f.send(cx));
    assert!(creates(&f, v).is_empty(), "no request, no draft");
    assert!(seen.borrow().is_empty());
    assert_eq!(f.read_with(v, |f, _| f.form.stage.clone()), Stage::Edit);
}

#[gpui::test]
fn the_writes_form_sends_a_contract_the_core_accepts_and_a_draft_is_never_approved(
    cx: &mut TestAppContext,
) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    // the app's folder is the start: its base commit is asked for at once
    assert_eq!(
        f.read_with(v, |f, _| f.outbox.clone()),
        vec![Out::Head(dir())]
    );
    head_known(v, &f);
    fill_writes(v, &f);
    let c = cmd(v);
    assert!(key(v, &f, "enter", c, false), "Cmd+Enter sends");
    let sent = creates(&f, v);
    let [
        Out::Submit(Submit::Create {
            workspace,
            base_commit,
            contract,
            key,
        }),
    ] = sent.as_slice()
    else {
        panic!("{sent:?}")
    };
    assert_eq!(
        (workspace.as_str(), base_commit.as_str()),
        (dir().as_str(), COMMIT)
    );
    assert_eq!(contract.kind, ContractKind::Write);
    assert!(key.starts_with("create-"));
    assert_eq!(f.read_with(v, |f, _| f.form.stage.clone()), Stage::Sending);
    // the server's draft: a review, and still nothing approved
    review(&f, v);
    assert!(
        creates(&f, v)
            .iter()
            .all(|o| !matches!(o, Out::Approve { .. }))
    );
    assert!(matches!(
        f.read_with(v, |f, _| f.form.stage.clone()),
        Stage::Review(_)
    ));
    assert!(v.debug_bounds("sa-goal-contract").is_some());
    assert!(v.debug_bounds("sa-goal-dialog").is_none());
}

#[gpui::test]
fn each_error_of_the_form_shows_under_its_own_field_and_the_text_is_kept(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    type_in(v, &f, Field::Goal, "Move login to OIDC");
    type_in(v, &f, Field::Folder, "relative/folder");
    f.update(v, |f, cx| f.send(cx));
    v.run_until_parked();
    for (field, id) in [
        (Field::Criteria, "sa-goal-criteria-error"),
        (Field::Checks, "sa-goal-checks-error"),
        (Field::Scope, "sa-goal-scope-error"),
        (Field::Budget, "sa-goal-budget-error"),
        (Field::Folder, "sa-goal-folder-error"),
    ] {
        assert!(
            !f.read_with(v, |f, _| f.errors_of(field).is_empty()),
            "{id}"
        );
        assert!(v.debug_bounds(id).is_some(), "{id} is drawn");
    }
    assert!(creates(&f, v).is_empty());
    assert_eq!(
        f.read_with(v, |f, cx| f.text(cx).goal),
        "Move login to OIDC",
        "an error never clears the text"
    );
    // the core's own words (not tied to a field) go above the form
    assert_eq!(field_of("version: must be 1"), None);
}

#[gpui::test]
fn the_read_only_form_has_its_own_fields_and_sends_a_read_only_contract_without_a_commit(
    cx: &mut TestAppContext,
) {
    let (f, v, _) = open(cx, GoalKind::ReadOnly);
    for id in [
        "sa-goal-goal",
        "sa-goal-criteria",
        "sa-goal-budget",
        "sa-goal-attempts",
        "sa-goal-folder",
    ] {
        assert!(v.debug_bounds(id).is_some(), "{id}");
    }
    for id in ["sa-goal-checks", "sa-goal-scope", "sa-goal-base"] {
        assert!(v.debug_bounds(id).is_none(), "{id} is for writes only");
    }
    assert!(
        v.debug_bounds("sa-goal-folder-note").is_some(),
        "the folder warning"
    );
    // no folder is preselected, and no HEAD is ever read
    assert_eq!(f.read_with(v, |f, cx| f.text(cx).workspace), "");
    assert!(f.read_with(v, |f, _| f.outbox.is_empty()));
    fill_read_only(v, &f);
    f.update(v, |f, cx| f.send(cx));
    let Some(Out::Submit(Submit::Create {
        base_commit,
        contract,
        ..
    })) = creates(&f, v).first().cloned()
    else {
        panic!("{:?}", f.read_with(v, |f, _| f.form.errors.clone()))
    };
    assert_eq!(base_commit, "");
    assert_eq!(contract.kind, ContractKind::ReadOnly);
    assert_eq!(contract.criteria.len(), 2, "report plus one rubric line");
}

#[gpui::test]
fn a_read_only_goal_needs_an_absolute_folder_and_a_budget(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::ReadOnly);
    type_in(v, &f, Field::Goal, "Review the module");
    f.update(v, |f, cx| f.send(cx));
    assert!(!f.read_with(v, |f, _| f.errors_of(Field::Budget).is_empty()));
    assert!(!f.read_with(v, |f, _| f.errors_of(Field::Folder).is_empty()));
    assert!(creates(&f, v).is_empty());
}

#[gpui::test]
fn approving_takes_the_dialog_a_wait_a_tab_to_the_button_and_a_fresh_enter(
    cx: &mut TestAppContext,
) {
    let (f, v, seen) = open(cx, GoalKind::Writes);
    // nothing to approve before the server stored a draft
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    assert!(f.read_with(v, |f, _| f.dialog.is_none()));
    review(&f, v);
    // opening the dialog approves nothing
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    v.run_until_parked();
    assert!(v.debug_bounds("sa-goal-dialog").is_some());
    assert!(
        creates(&f, v)
            .iter()
            .all(|o| !matches!(o, Out::Approve { .. }))
    );
    // a click at once, and Enter with no tab to the button: nothing
    f.update_in(v, |f, w, cx| f.approve(false, false, w, cx));
    key(v, &f, "enter", none(), false);
    // Tab reaches the button; before 400 ms it is still inert
    key(v, &f, "tab", none(), false);
    assert!(f.update_in(v, |f, w, _| f.stops.dlg_approve.is_focused(w)));
    key(v, &f, "enter", none(), false);
    assert!(creates(&f, v).is_empty(), "inside the 400 ms");
    f.update(v, |f, _| {
        f.dialog.as_mut().unwrap().since -= Duration::from_millis(500)
    });
    // a held Enter never approves
    key(v, &f, "enter", none(), true);
    assert!(creates(&f, v).is_empty());
    // Cmd+Enter is Send, never approve
    let c = cmd(v);
    key(v, &f, "enter", c, false);
    assert!(creates(&f, v).is_empty());
    // one deliberate press: the version and hash the server reported
    key(v, &f, "enter", none(), false);
    let r = sample::review(false);
    assert_eq!(
        creates(&f, v),
        vec![Out::Approve {
            outcome: r.outcome.clone(),
            version: r.version,
            hash: r.hash.clone(),
            key: r.key.clone(),
        }]
    );
    // a second press while it is in flight sends nothing more
    key(v, &f, "enter", none(), false);
    assert_eq!(creates(&f, v).len(), 1);
    // the server applied it: the shell is told which outcome
    f.update(v, |f, cx| {
        f.apply_form(FormMsg::Approved("Contract v1: applied".into()), cx)
    });
    assert_eq!(*seen.borrow(), vec![format!("approved {}", r.outcome)]);
}

#[gpui::test]
fn a_review_the_server_cannot_back_up_cannot_be_approved(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    let mut r = sample::review(false);
    r.contract = Err("unreadable".into());
    f.update(v, |f, cx| f.apply_form(FormMsg::Review(Box::new(r)), cx));
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    assert!(f.read_with(v, |f, _| f.dialog.is_none()));
}

#[gpui::test]
fn a_refusal_brings_the_review_back_with_the_dialog_closed(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    review(&f, v);
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    f.update(v, |f, cx| {
        f.apply_form(
            FormMsg::Failed {
                text: "Not approved: stale".into(),
                spent: true,
            },
            cx,
        )
    });
    v.run_until_parked();
    assert!(f.read_with(v, |f, _| f.dialog.is_none()));
    assert!(matches!(
        f.read_with(v, |f, _| f.form.stage.clone()),
        Stage::Review(_)
    ));
    assert!(v.debug_bounds("sa-goal-errors").is_some());
}

#[gpui::test]
fn native_windows_off_and_an_unready_engine_each_say_so(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    f.update(v, |f, cx| f.apply(Msg::Screen(Screen::Windows), cx));
    v.run_until_parked();
    assert!(v.debug_bounds("sa-goal-blocked").is_some());
    assert!(
        v.debug_bounds("sa-goal-goal").is_none(),
        "no form on native Windows"
    );
    assert_eq!(
        f.read_with(v, |f, _| f.blocked()),
        Some(synthesis_view::outcomes::WSL2_REFUSAL)
    );
    f.update(v, |f, cx| f.apply(Msg::Screen(Screen::Off), cx));
    assert!(f.read_with(v, |f, _| f.blocked().is_some_and(|b| b.contains("off"))));
    // an engine that does not advertise read-only goals refuses that form only
    f.update(v, |f, cx| f.apply(Msg::Screen(Screen::Ready), cx));
    assert_eq!(f.read_with(v, |f, _| f.blocked()), None);
    let (g, v2, _) = open(cx, GoalKind::ReadOnly);
    g.update(v2, |g, cx| g.apply(Msg::ReadOnlyGoal(false), cx));
    type_in(v2, &g, Field::Goal, "Review the module");
    assert_eq!(
        g.read_with(v2, |g, cx| g.why_not_send(cx)),
        Some(model::READ_ONLY_SOON)
    );
    // an unreachable link keeps the form and the text, and Send is off
    let (h, v3, _) = open(cx, GoalKind::Writes);
    type_in(v3, &h, Field::Goal, "Keep me");
    h.update(v3, |h, cx| {
        h.apply(Msg::Screen(Screen::Unreachable("no endpoint".into())), cx)
    });
    v3.run_until_parked();
    assert!(v3.debug_bounds("sa-goal-notice").is_some());
    assert!(h.read_with(v3, |h, cx| h.why_not_send(cx)).is_some());
    assert_eq!(h.read_with(v3, |h, cx| h.text(cx).goal), "Keep me");
}

#[gpui::test]
fn a_prefill_fills_the_text_only_and_never_over_what_the_person_typed(cx: &mut TestAppContext) {
    let (f, v, seen) = open(cx, GoalKind::ReadOnly);
    let ok = f.update_in(v, |f, w, cx| {
        f.prefill("Audit the logger\u{202e}uses", &dir(), w, cx)
    });
    assert!(ok);
    let t = f.read_with(v, |f, cx| f.text(cx));
    assert!(
        t.goal.starts_with("Audit the logger") && !t.goal.contains('\u{202e}'),
        "{:?}",
        t.goal
    );
    assert_eq!(t.workspace, dir());
    // text only: no request, no stage change, nobody approved
    assert!(creates(&f, v).is_empty());
    assert_eq!(f.read_with(v, |f, _| f.form.stage.clone()), Stage::Edit);
    assert!(seen.borrow().is_empty());
    // a second prefill never overwrites the goal that is there
    assert!(!f.update_in(v, |f, w, cx| f.prefill("Something else", "", w, cx)));
    assert!(f.read_with(v, |f, cx| f.text(cx).goal.starts_with("Audit")));
}

#[gpui::test]
fn esc_closes_the_dialog_first_then_the_frame_and_tab_moves_on(cx: &mut TestAppContext) {
    let (f, v, seen) = open(cx, GoalKind::Writes);
    review(&f, v);
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    key(v, &f, "escape", none(), false);
    assert!(f.read_with(v, |f, _| f.dialog.is_none()));
    assert!(seen.borrow().is_empty(), "the frame stays");
    key(v, &f, "escape", none(), false);
    assert_eq!(*seen.borrow(), vec!["close".to_string()]);
    // the keys of the typed text are not the form's
    assert!(!key(v, &f, "a", none(), false));
}

#[test]
fn the_approve_rule_is_the_cards_fresh_press_after_400_ms() {
    let t0 = Instant::now();
    let later = t0 + Duration::from_millis(450);
    assert!(!approve_ok(
        t0,
        t0 + Duration::from_millis(100),
        false,
        false,
        false
    ));
    assert!(approve_ok(t0, later, false, false, false), "a click");
    assert!(!approve_ok(t0, later, true, true, true), "held");
    assert!(
        !approve_ok(t0, later, false, true, false),
        "Enter off the button"
    );
    assert!(approve_ok(t0, later, false, true, true));
}

#[test]
fn send_is_off_until_a_goal_is_typed_and_while_sending() {
    let f = Form::new_outcome("/w");
    let ready = Screen::Ready;
    assert!(why_not_send(GoalKind::Writes, &f, &ready, true, "  ").is_some());
    assert_eq!(why_not_send(GoalKind::Writes, &f, &ready, true, "x"), None);
    assert_eq!(
        why_not_send(GoalKind::Writes, &f, &Screen::Loading, true, "x"),
        Some("Connecting to the orchestrator...")
    );
}

#[gpui::test]
fn space_on_the_focused_approve_button_never_approves_and_shift_tab_moves(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::Writes);
    review(&f, v);
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    f.update(v, |f, _| {
        f.dialog.as_mut().unwrap().since -= Duration::from_millis(500)
    });
    key(v, &f, "tab", none(), false);
    assert!(f.update_in(v, |f, w, _| f.stops.dlg_approve.is_focused(w)));
    // a keyboard click (Space, Enter key-up) is not a path to approval
    v.simulate_keystrokes("space");
    v.simulate_event(gpui::KeyUpEvent {
        keystroke: Keystroke {
            modifiers: none(),
            key: "space".into(),
            key_char: None,
        },
    });
    v.run_until_parked();
    assert!(creates(&f, v).is_empty(), "{:?}", creates(&f, v));
    // Shift+Tab leaves the button for the other one
    let shift = Modifiers {
        shift: true,
        ..none()
    };
    key(v, &f, "tab", shift, false);
    assert!(f.update_in(v, |f, w, _| f.stops.dlg_cancel.is_focused(w)));
}

#[gpui::test]
fn esc_and_close_wait_while_an_approval_is_in_flight(cx: &mut TestAppContext) {
    let (f, v, seen) = open(cx, GoalKind::Writes);
    review(&f, v);
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    f.update(v, |f, _| {
        f.dialog.as_mut().unwrap().since -= Duration::from_millis(500)
    });
    key(v, &f, "tab", none(), false);
    key(v, &f, "enter", none(), false);
    assert_eq!(creates(&f, v).len(), 1);
    key(v, &f, "escape", none(), false);
    key(v, &f, "escape", none(), false);
    assert!(seen.borrow().is_empty(), "the frame waits for the answer");
}

#[gpui::test]
fn the_dialog_of_a_read_only_goal_carries_the_folder_warning(cx: &mut TestAppContext) {
    let (f, v, _) = open(cx, GoalKind::ReadOnly);
    review(&f, v);
    f.update_in(v, |f, w, cx| f.open_dialog(w, cx));
    v.run_until_parked();
    assert!(v.debug_bounds("sa-goal-dialog-note").is_some());
}
