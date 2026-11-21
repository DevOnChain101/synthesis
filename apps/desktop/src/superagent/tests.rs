use gpui::{
    Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};

use super::frames::{Frame, GoalKind, MAX_SESSIONS};
use super::*;

fn open(cx: &mut TestAppContext) -> (Entity<SuperAgentView>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (sa, v) = cx.add_window_view(|window, cx| {
        let o = cx.new(OutcomesView::sample);
        let mut sa = SuperAgentView::new(o, false, cx);
        sa.shown(window, cx);
        sa
    });
    v.simulate_resize(size(px(1400.), px(900.)));
    v.run_until_parked();
    (sa, v)
}

fn press(v: &mut VisualTestContext, sa: &Entity<SuperAgentView>, key: &str, m: Modifiers) -> bool {
    let ev = KeyDownEvent {
        keystroke: Keystroke {
            modifiers: m,
            key: key.into(),
            key_char: None,
        },
        is_held: false,
        prefer_character_input: false,
    };
    let r = sa.update_in(v, |s, w, cx| s.on_key(&ev, w, cx));
    v.run_until_parked();
    r
}

fn none() -> Modifiers {
    Modifiers::default()
}

fn cmd(v: &mut VisualTestContext) -> Modifiers {
    v.update(|_, cx| Modifiers::secondary_key(cx))
}

fn cursor(v: &mut VisualTestContext, sa: &Entity<SuperAgentView>) -> Option<Option<usize>> {
    let o = sa.read_with(v, |s, _| s.outcomes.clone());
    o.read_with(v, |o, _| o.state.picker.as_ref().map(|p| p.cursor))
}

fn active(v: &mut VisualTestContext, sa: &Entity<SuperAgentView>) -> Frame {
    sa.read_with(v, |s, _| s.frames.active().clone())
}

fn width(v: &mut VisualTestContext) -> f32 {
    f32::from(v.debug_bounds("sa-rail").unwrap().size.width)
}

#[gpui::test]
fn the_picker_starts_with_nothing_chosen_and_enter_does_nothing_until_a_row_is(
    cx: &mut TestAppContext,
) {
    let (sa, v) = open(cx);
    assert!(v.debug_bounds("sa-home").is_some(), "the home placeholder");
    assert!(press(v, &sa, "n", none()));
    assert_eq!(cursor(v, &sa), Some(None), "open, nothing preselected");
    assert!(v.debug_bounds("sa-kind-picker").is_some());
    press(v, &sa, "enter", none());
    assert_eq!(cursor(v, &sa), Some(None), "Enter is inert");
    assert_eq!(sa.read_with(v, |s, _| s.frames.tabs().len()), 1);
    press(v, &sa, "3", none());
    assert_eq!(cursor(v, &sa), Some(Some(2)));
    press(v, &sa, "escape", none());
    assert_eq!(cursor(v, &sa), None, "Esc closes");
    // the rows: New goal (writes) opens its frame as a tab
    press(v, &sa, "n", none());
    press(v, &sa, "2", none());
    press(v, &sa, "enter", none());
    assert_eq!(cursor(v, &sa), None);
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
}

#[gpui::test]
fn a_session_row_that_cannot_make_its_folder_says_so_and_opens_nothing(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    press(v, &sa, "n", none());
    press(v, &sa, "1", none());
    press(v, &sa, "enter", none());
    // the sample has no home to make a scratch folder in: the picker stays with the reason
    assert_eq!(cursor(v, &sa), Some(Some(0)));
    assert!(v.debug_bounds("sa-kind-notice").is_some());
    assert_eq!(sa.read_with(v, |s, _| s.frames.tabs().len()), 1);
}

#[gpui::test]
fn the_key_map_of_the_mock(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    let c = cmd(v);
    // Cmd+T opens the picker, Esc closes it
    assert!(press(v, &sa, "t", c));
    assert_eq!(cursor(v, &sa), Some(None));
    press(v, &sa, "escape", none());
    // two frames, then Ctrl+Tab and Ctrl+Shift+Tab
    sa.update(v, |s, cx| {
        s.open(Frame::Outcome("a".into()), cx);
        s.open(Frame::NewGoal(GoalKind::ReadOnly), cx);
    });
    let ctrl = Modifiers {
        control: true,
        ..none()
    };
    press(v, &sa, "tab", ctrl);
    assert_eq!(active(v, &sa), Frame::Home, "wraps to the pinned home");
    press(
        v,
        &sa,
        "tab",
        Modifiers {
            shift: true,
            ..ctrl
        },
    );
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::ReadOnly));
    // Alt+Left and Alt+Right walk the history
    let alt = Modifiers {
        alt: true,
        ..none()
    };
    press(v, &sa, "left", alt);
    assert_eq!(active(v, &sa), Frame::Home);
    press(v, &sa, "right", alt);
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::ReadOnly));
    // Cmd+W closes the active tab but never the home
    press(v, &sa, "w", c);
    assert_eq!(sa.read_with(v, |s, _| s.frames.tabs().len()), 2);
    sa.update(v, |s, cx| s.focus_tab(0, cx));
    press(v, &sa, "w", c);
    assert_eq!(
        sa.read_with(v, |s, _| s.frames.tabs().len()),
        2,
        "home stays"
    );
    // Cmd+B collapses the rail and expands it
    assert_eq!(width(v), 248.);
    press(v, &sa, "b", c);
    assert_eq!(width(v), 72.);
    press(v, &sa, "b", c);
    assert_eq!(width(v), 248.);
    // an unrelated chord is not ours
    assert!(!press(v, &sa, "x", c));
}

#[gpui::test]
fn the_rail_has_a_roving_cursor_and_enter_opens_the_row(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    sa.update_in(v, |s, w, cx| w.focus(&s.rail_focus, cx));
    v.run_until_parked();
    // New is row 0, the home row 1; the sample's outcomes follow
    press(v, &sa, "down", none());
    assert_eq!(sa.read_with(v, |s, _| s.rail_cursor), 2);
    press(v, &sa, "enter", none());
    assert!(matches!(active(v, &sa), Frame::Outcome(_)));
    // the focus went to the frame; the rail keys are not taken any more
    assert!(!press(v, &sa, "down", none()));
    sa.update_in(v, |s, w, cx| w.focus(&s.rail_focus, cx));
    press(v, &sa, "home", none());
    press(v, &sa, "enter", none());
    assert_eq!(
        cursor(v, &sa),
        Some(None),
        "Home row is New: the picker opens"
    );
}

#[gpui::test]
fn at_most_six_session_tabs_and_the_plus_says_why(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    for i in 0..MAX_SESSIONS {
        sa.update(v, |s, cx| {
            s.open_session_folder(std::path::Path::new(&format!("/w/p{i}")), cx)
        });
    }
    v.run_until_parked();
    assert_eq!(
        sa.read_with(v, |s, _| s.frames.session_count()),
        MAX_SESSIONS
    );
    // a seventh is refused with the reason, the rail still lists only what exists
    sa.update(v, |s, cx| {
        s.open_session_folder(std::path::Path::new("/w/extra"), cx)
    });
    assert_eq!(
        sa.read_with(v, |s, _| s.frames.session_count()),
        MAX_SESSIONS
    );
    assert_eq!(
        sa.read_with(v, |s, _| s.notice.clone()),
        Some(CAP_NOTE.into())
    );
    // the picker's session row refuses too, before any folder is made
    press(v, &sa, "n", none());
    press(v, &sa, "1", none());
    press(v, &sa, "enter", none());
    assert_eq!(cursor(v, &sa), Some(Some(0)));
    assert!(v.debug_bounds("sa-kind-notice").is_some());
    // closing one frees a place
    sa.update(v, |s, cx| s.close_tab(1, cx));
    assert!(sa.read_with(v, |s, _| s.frames.session_count()) < MAX_SESSIONS);
}

#[gpui::test]
fn a_session_tab_shows_the_folder_name_and_closes_from_its_x(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    sa.update(v, |s, cx| {
        s.open_session_folder(std::path::Path::new("/w/api-gateway"), cx)
    });
    v.run_until_parked();
    assert_eq!(
        sa.read_with(v, |s, cx| s
            .title(&Frame::Session("/w/api-gateway".into()), cx)),
        "api-gateway"
    );
    let x = v.debug_bounds("sa-tab-1-x").unwrap().center();
    v.simulate_click(x, Default::default());
    v.run_until_parked();
    assert_eq!(
        sa.read_with(v, |s, _| s.frames.tabs().len()),
        1,
        "the tab closed"
    );
    assert_eq!(
        sa.read_with(v, |s, _| s.sessions.len()),
        1,
        "the rail row stays"
    );
}
