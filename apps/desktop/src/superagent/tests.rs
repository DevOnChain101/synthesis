use gpui::{
    Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};

use super::frames::{Frame, GoalKind, MAX_SESSIONS};
use super::session::host::SessionHost;
use super::*;

fn open(cx: &mut TestAppContext) -> (Entity<SuperAgentView>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (sa, v) = cx.add_window_view(|window, cx| {
        let o = cx.new(OutcomesView::sample);
        let host = cx.new(|cx| SessionHost::new(None, window, cx));
        let mut sa = SuperAgentView::new(o, host, None, false, window, cx);
        sa.shown(window, cx);
        sa
    });
    v.simulate_resize(size(px(1400.), px(900.)));
    v.run_until_parked();
    (sa, v)
}

/// A session tab without a hosted chat (the sample hosts none): the cap, the titles and the close need only frames.
fn session_tab(s: &mut SuperAgentView, folder: &str, cx: &mut Context<SuperAgentView>) {
    let id = format!("s{}", s.next_session);
    s.register_session(&id, std::path::Path::new(folder));
    s.open(Frame::Session(id), cx);
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
    assert!(v.debug_bounds("sa-home").is_some(), "the home view");
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
        sa.update(v, |s, cx| session_tab(s, &format!("/w/p{i}"), cx));
    }
    v.run_until_parked();
    assert_eq!(
        sa.read_with(v, |s, _| s.frames.session_count()),
        MAX_SESSIONS
    );
    // a seventh is refused with the reason, the rail still lists only what exists
    sa.update(v, |s, cx| session_tab(s, "/w/extra", cx));
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
    sa.update(v, |s, cx| session_tab(s, "/w/api-gateway", cx));
    v.run_until_parked();
    assert_eq!(
        sa.read_with(v, |s, cx| s.title(&Frame::Session("s1".into()), cx)),
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

fn open_live(
    cx: &mut TestAppContext,
    cfg: crate::tui_view::driver::LiveCfg,
) -> (Entity<SuperAgentView>, &mut VisualTestContext) {
    cx.update(gpui_component::init);
    let (sa, v) = cx.add_window_view(|window, cx| {
        let o = cx.new(OutcomesView::sample);
        let host = cx.new(|cx| SessionHost::new(Some(cfg), window, cx));
        let mut sa = SuperAgentView::new(o, host, None, false, window, cx);
        sa.shown(window, cx);
        sa
    });
    v.simulate_resize(size(px(1400.), px(900.)));
    v.run_until_parked();
    (sa, v)
}

fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
    let mut f = std::pin::pin!(f);
    let mut c = std::task::Context::from_waker(futures::task::noop_waker_ref());
    match f.as_mut().poll(&mut c) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("not ready"),
    }
}

#[gpui::test]
fn the_conductors_port_starts_reads_sends_and_closes_the_sessions_of_this_window(
    cx: &mut TestAppContext,
) {
    use super::chat::glue::WindowPort as _;
    use synthesis_app::conductor::SessionStart;
    use synthesis_harness::PersonApproved;
    let (f, cfg) = super::session::host::tests::fx(2);
    let (sa, v) = open_live(cx, cfg);
    let port = sa.read_with(v, |s, _| s.port.clone());
    assert!(port.sessions().is_empty());
    let folder = f.base.join("f1");
    let req = SessionStart {
        first_message: "hello".into(),
        folder: Some(folder.to_string_lossy().into_owned()),
        model: None,
        title: None,
    };
    let fut = port.start(&req, PersonApproved::for_text("hello"));
    v.run_until_parked();
    assert_eq!(ready(fut).unwrap(), "s1");
    assert_eq!(
        active(v, &sa),
        Frame::Session("s1".into()),
        "it opens as a tab, never in the TUI view"
    );
    let rows = port.sessions();
    assert_eq!(
        (rows.len(), rows[0].handle.as_str(), rows[0].live),
        (1, "s1", true)
    );
    assert!(port.report("s1").is_some() && port.report("s9").is_none());
    // a line to an unknown handle is refused; Stop reaches the open one
    let fut = port.send("s9", "hi", PersonApproved::for_text("hi"));
    v.run_until_parked();
    assert_eq!(ready(fut).unwrap_err(), "no such session");
    let fut = port.interrupt("s1");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    // a line to a session with no running task is refused by the engine, never typed as the person's own
    let fut = port.send("s1", "hi", PersonApproved::for_text("hi"));
    v.run_until_parked();
    assert!(ready(fut).is_err());
    // close: the tab goes, the row stays as ended
    let fut = port.close("s1");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 0);
    let rows = port.sessions();
    assert_eq!((rows[0].live, rows[0].state.as_str()), (false, "ended"));
    // the same folder again is a NEW session with a NEW handle; the old handle stays dead
    let fut = port.start(&req, PersonApproved::for_text("hello"));
    v.run_until_parked();
    assert_eq!(ready(fut).unwrap(), "s2");
    let fut = port.send("s1", "stale", PersonApproved::for_text("stale"));
    v.run_until_parked();
    assert!(
        ready(fut).is_err(),
        "a stale card never reaches the new session"
    );
    let fut = port.close("s1");
    v.run_until_parked();
    assert!(ready(fut).is_err());
    // a start whose folder already has an open session is refused: its line is never typed into that session
    let fut = port.start(&req, PersonApproved::for_text("hello"));
    v.run_until_parked();
    assert!(ready(fut).unwrap_err().contains("already open"));
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 1);
    let fut = port.close("s2");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    // a model the card names cannot be applied: refused, not ignored
    let with_model = SessionStart {
        model: Some("x/y".into()),
        ..req.clone()
    };
    let fut = port.start(&with_model, PersonApproved::for_text("hello"));
    v.run_until_parked();
    assert!(ready(fut).unwrap_err().contains("model"));
    // a bad folder says why
    let bad = SessionStart {
        folder: Some(f.base.join("missing").to_string_lossy().into_owned()),
        ..req
    };
    let fut = port.start(&bad, PersonApproved::for_text("hello"));
    v.run_until_parked();
    assert!(ready(fut).unwrap_err().starts_with("no such folder"));
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 0);
    v.update(|_, _| ());
    v.run_until_parked();
    super::session::host::tests::wait_free(&f.home);
}

#[gpui::test]
fn the_conductors_goal_card_opens_the_prefilled_form_and_its_end_ends_the_frame(
    cx: &mut TestAppContext,
) {
    use synthesis_app::conductor::GoalDraft;
    let (sa, v) = open(cx);
    let draft = GoalDraft {
        kind: "writes".into(),
        contract: "Make the build pass".into(),
        folder: "/w/api".into(),
        base_commit: None,
    };
    // the real path: the executor awaits the effect in a foreground task of the UI thread, which is also the
    // thread that serves the command (a blocking wait there would hang)
    let port = sa.read_with(v, |s, _| s.port.clone());
    let fx = super::chat::glue::DesktopEffects {
        port,
        outcomes: std::sync::Arc::new(
            super::chat::glue::OutcomeWorker::start(std::env::temp_dir()),
        ),
    };
    let via = synthesis_app::conductor::Via {
        proposal_id: "p1".into(),
        proposal_sha256: String::new(),
    };
    let task = v.update(|_, cx| {
        use synthesis_app::conductor::Effects as _;
        let draft = draft.clone();
        cx.spawn(async move |_| fx.create_goal(&draft, &via).await)
    });
    v.run_until_parked();
    assert_eq!(ready(task).unwrap(), "New goal form");
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
    let o = sa.read_with(v, |s, _| s.outcomes.clone());
    let init = o.read_with(v, |o, _| o.state.form.as_ref().map(|f| f.initial.clone()));
    let init = init.expect("the form is open");
    assert_eq!(
        (init.goal.as_str(), init.workspace.as_str()),
        ("Make the build pass", "/w/api")
    );
    assert!(
        v.debug_bounds("sa-frame").is_some(),
        "the frame is under the modal"
    );
    // the form closed: its frame goes with it
    o.update(v, |o, cx| {
        o.state.close_form();
        cx.notify();
    });
    v.run_until_parked();
    assert_eq!(active(v, &sa), Frame::Home);
    // a read-only goal has no form yet: the frame says so and nothing is created
    let ro = GoalDraft {
        kind: "read_only".into(),
        contract: "x".into(),
        folder: "/w".into(),
        base_commit: None,
    };
    let port = sa.read_with(v, |s, _| s.port.clone());
    let fut = super::chat::glue::WindowPort::open_goal_form(&*port, &ro);
    v.run_until_parked();
    assert!(ready(fut).unwrap().contains("not built yet"));
    assert!(o.read_with(v, |o, _| o.state.form.is_none()));
}

#[gpui::test]
fn the_kind_pickers_goal_row_opens_the_form_and_the_enable_dialog_closes_with_a_recheck(
    cx: &mut TestAppContext,
) {
    let (sa, v) = open(cx);
    press(v, &sa, "n", none());
    press(v, &sa, "2", none());
    press(v, &sa, "enter", none());
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
    let o = sa.read_with(v, |s, _| s.outcomes.clone());
    assert!(
        o.read_with(v, |o, _| o.state.form.is_some()),
        "the picker's row opens the form too"
    );
    // the chat's "Turn on" asks for the enable dialog; the sample is Ready, so only the note comes
    sa.update(v, |s, cx| s.enable_guide(cx));
    assert!(sa.read_with(v, |s, _| s.notice.is_some()));
}
