use gpui::{
    Entity, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext, px, size,
};

use super::frames::{Frame, GoalKind};
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
fn many_session_tabs_are_all_kept_and_the_focused_one_stays_reachable(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    for i in 0..12 {
        sa.update(v, |s, cx| session_tab(s, &format!("/w/p{i}"), cx));
    }
    v.run_until_parked();
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 12);
    // the strip scrolls; the last opened is the focused one, and an earlier one can be focused again
    sa.update(v, |s, cx| s.focus_tab(2, cx));
    assert_eq!(sa.read_with(v, |s, _| s.frames.active_index()), 2);
    assert_eq!(sa.read_with(v, |s, _| s.notice.clone()), None);
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

/// The conductor's START effect (it carries no approval): the command the port sends, run on the UI thread.
fn start(
    sa: &Entity<SuperAgentView>,
    v: &mut VisualTestContext,
    req: &synthesis_app::conductor::SessionStart,
) -> Result<String, String> {
    let (tx, mut rx) = futures::channel::oneshot::channel();
    sa.update_in(v, |s, w, cx| s.run_cmd(Cmd::Start(req.clone(), tx), w, cx));
    v.run_until_parked();
    rx.try_recv().unwrap().unwrap()
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
    let (f, cfg) = super::session::host::tests::fx(2);
    cx.executor().allow_parking();
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
    let r = start(&sa, v, &req);
    assert_eq!(r.unwrap(), "s1");
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
    // an unknown handle is refused; Stop reaches the open one
    let fut = port.interrupt("s9");
    v.run_until_parked();
    assert_eq!(ready(fut).unwrap_err(), "no such session");
    let fut = port.interrupt("s1");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    // close: the tab goes, the row stays as ended
    let fut = port.close("s1");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 0);
    let rows = port.sessions();
    assert_eq!((rows[0].live, rows[0].state.as_str()), (false, "ended"));
    // the same folder again is a NEW session with a NEW handle; the old handle stays dead
    let r = start(&sa, v, &req);
    assert_eq!(r.unwrap(), "s2");
    let fut = port.interrupt("s1");
    v.run_until_parked();
    assert!(
        ready(fut).is_err(),
        "a stale card never reaches the new session"
    );
    let fut = port.close("s1");
    v.run_until_parked();
    assert!(ready(fut).is_err());
    // a start whose folder already has an open session is refused: its line is never typed into that session
    let r = start(&sa, v, &req);
    assert!(r.unwrap_err().contains("already open"));
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 1);
    let fut = port.close("s2");
    v.run_until_parked();
    assert!(ready(fut).is_ok());
    // a model the card names cannot be applied: refused, not ignored
    let with_model = SessionStart {
        model: Some("x/y".into()),
        ..req.clone()
    };
    let r = start(&sa, v, &with_model);
    assert!(r.unwrap_err().contains("model"));
    // a bad folder says why
    let bad = SessionStart {
        folder: Some(f.base.join("missing").to_string_lossy().into_owned()),
        ..req
    };
    let r = start(&sa, v, &bad);
    assert!(r.unwrap_err().starts_with("no such folder"));
    assert_eq!(sa.read_with(v, |s, _| s.frames.session_count()), 0);
    v.update(|_, _| ());
    v.run_until_parked();
    super::session::host::tests::wait_free(&f.home);
}

/// The conductor's goal-card effect, as the executor runs it (a foreground task of the UI thread).
fn card(
    sa: &Entity<SuperAgentView>,
    v: &mut VisualTestContext,
    kind: &str,
    text: &str,
) -> Result<String, String> {
    use synthesis_app::conductor::GoalDraft;
    let draft = GoalDraft {
        kind: kind.into(),
        contract: text.into(),
        folder: "/w/api".into(),
        base_commit: None,
    };
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
        cx.spawn(async move |_| fx.create_goal(&draft, &via).await)
    });
    v.run_until_parked();
    ready(task)
}

fn goal_form(
    sa: &Entity<SuperAgentView>,
    v: &mut VisualTestContext,
    k: GoalKind,
) -> Option<Entity<goal::GoalForm>> {
    sa.read_with(v, |s, _| {
        s.goals.iter().find(|(g, ..)| *g == k).map(|g| g.1.clone())
    })
}

fn goal_text(f: &Entity<goal::GoalForm>, v: &mut VisualTestContext) -> String {
    f.read_with(v, |f, cx| f.ui.goal.read(cx).value().to_string())
}

#[gpui::test]
fn the_conductors_goal_card_fills_the_form_and_approves_nothing(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    assert_eq!(
        card(&sa, v, "writes", "Make the build pass").unwrap(),
        "New goal form"
    );
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
    assert!(
        v.debug_bounds("sa-goal").is_some(),
        "the new form is the body"
    );
    let f = goal_form(&sa, v, GoalKind::Writes).expect("one form");
    assert_eq!(goal_text(&f, v), "Make the build pass");
    f.read_with(v, |f, _| {
        assert!(
            f.outbox
                .iter()
                .all(|o| !matches!(o, goal::Out::Approve { .. }))
        );
        assert!(!f.approving());
    });
    // a second card never overwrites what is there
    f.update_in(v, |f, w, cx| {
        f.set_text(goal::model::Field::Goal, "mine", w, cx)
    });
    let r = card(&sa, v, "writes", "other").unwrap();
    assert!(r.contains("nothing was overwritten"), "{r}");
    assert_eq!(goal_text(&f, v), "mine");
    assert_eq!(
        sa.read_with(v, |s, _| s.goals.len()),
        1,
        "the same form, not a second"
    );
    // a read-only card opens the read-only form
    assert_eq!(
        card(&sa, v, "read_only", "x").unwrap(),
        "New read-only goal form"
    );
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::ReadOnly));
}

#[gpui::test]
fn the_pickers_goal_rows_open_the_new_form_not_the_modal_and_read_only_needs_the_capability(
    cx: &mut TestAppContext,
) {
    let (sa, v) = open(cx);
    let o = sa.read_with(v, |s, _| s.outcomes.clone());
    o.update(v, |o, _| o.state.read_only_goal = false);
    press(v, &sa, "n", none());
    press(v, &sa, "3", none());
    press(v, &sa, "enter", none());
    assert_eq!(
        active(v, &sa),
        Frame::Home,
        "disabled without the capability"
    );
    assert!(v.debug_bounds("sa-kind-notice").is_some());
    o.update(v, |o, _| o.state.read_only_goal = true);
    press(v, &sa, "enter", none());
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::ReadOnly));
    assert!(v.debug_bounds("sa-new-goal").is_some());
    let m = cmd(v);
    press(v, &sa, "t", m);
    press(v, &sa, "2", none());
    press(v, &sa, "enter", none());
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
    assert!(v.debug_bounds("sa-goal").is_some());
    assert!(
        o.read_with(v, |o, _| o.state.form.is_none()),
        "not the old modal"
    );
    assert_eq!(sa.read_with(v, |s, _| s.goals.len()), 2);
}

#[gpui::test]
fn approved_opens_the_outcome_and_closing_the_frame_drops_the_form(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    sa.update(v, |s, cx| s.open(Frame::NewGoal(GoalKind::Writes), cx));
    v.run_until_parked();
    let f = goal_form(&sa, v, GoalKind::Writes).expect("made when shown");
    f.update(v, |_, cx| {
        cx.emit(goal::GoalEvent::Approved {
            outcome: "o1".into(),
            note: String::new(),
        })
    });
    v.run_until_parked();
    assert_eq!(active(v, &sa), Frame::Outcome("o1".into()));
    assert!(!sa.read_with(v, |s, _| {
        s.frames.tabs().contains(&Frame::NewGoal(GoalKind::Writes))
    }));
    assert!(
        goal_form(&sa, v, GoalKind::Writes).is_none(),
        "dropped with its frame"
    );
    // Close (Esc, Cancel) closes the frame and drops the form too
    sa.update(v, |s, cx| s.open(Frame::NewGoal(GoalKind::ReadOnly), cx));
    v.run_until_parked();
    let f = goal_form(&sa, v, GoalKind::ReadOnly).unwrap();
    f.update(v, |_, cx| cx.emit(goal::GoalEvent::Close));
    v.run_until_parked();
    assert!(goal_form(&sa, v, GoalKind::ReadOnly).is_none());
    assert_eq!(sa.read_with(v, |s, _| s.frames.tabs().len()), 2);
}

#[gpui::test]
fn a_form_with_an_approval_in_flight_keeps_its_frame_and_says_so(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    sa.update(v, |s, cx| s.open(Frame::NewGoal(GoalKind::Writes), cx));
    v.run_until_parked();
    let f = goal_form(&sa, v, GoalKind::Writes).unwrap();
    f.update(v, |f, cx| {
        f.apply_form(
            crate::outcomes_view::link::FormMsg::Review(Box::new(
                crate::outcomes_view::sample::review(false),
            )),
            cx,
        );
        f.form.stage = crate::outcomes_view::form::Stage::Approving(
            match std::mem::replace(&mut f.form.stage, crate::outcomes_view::form::Stage::Edit) {
                crate::outcomes_view::form::Stage::Review(r) => r,
                _ => unreachable!(),
            },
        );
    });
    assert!(f.read_with(v, |f, _| f.approving()));
    // Cmd+W, a Close event and the tab's x all leave it open
    let m = cmd(v);
    press(v, &sa, "w", m);
    f.update(v, |_, cx| cx.emit(goal::GoalEvent::Close));
    v.run_until_parked();
    assert_eq!(active(v, &sa), Frame::NewGoal(GoalKind::Writes));
    assert!(sa.read_with(v, |s, _| s.notice.is_some()));
    // the server's answer then opens the outcome
    f.update(v, |_, cx| {
        cx.emit(goal::GoalEvent::Approved {
            outcome: "o2".into(),
            note: String::new(),
        })
    });
    v.run_until_parked();
    assert_eq!(active(v, &sa), Frame::Outcome("o2".into()));
}

// ------------------------------------------------------------------------------------------ the right sidebar (C2)

mod side {
    use super::*;
    use crate::superagent::sidebar::{Segment, Side};
    use crate::tui_view::ChatTab;
    use gpui::ClipboardItem;
    use synthesis_app::conductor::SessionStart;

    /// Secret-shaped, so `Redactor` would catch it in a digest; here it must never get that far.
    const CANARY: &str = "sk-ant-api03-SIDEBARCANARY0123456789abcdefghijkl";

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }

    /// Two hosted sessions s1 (f0) and s2 (f1); s2 is in front.
    fn two(
        cx: &mut TestAppContext,
    ) -> (
        super::super::session::host::tests::Fx,
        Entity<SuperAgentView>,
        &mut VisualTestContext,
    ) {
        // the engine builds on its own thread and delivers back to the window (and the last-shell test reopens a
        // real shell): activity from other threads is expected here
        cx.executor().allow_parking();
        let (f, cfg) = super::super::session::host::tests::fx(2);
        let (sa, v) = open_live(cx, cfg);
        for i in 0..2 {
            let req = SessionStart {
                first_message: "hi".into(),
                folder: Some(f.base.join(format!("f{i}")).to_string_lossy().into_owned()),
                model: None,
                title: None,
            };
            start(&sa, v, &req).unwrap();
        }
        // a scripted shell in each pane: the pane never starts a real PTY (its reader thread would wake the entity
        // from another thread, which the test scheduler refuses)
        let views = sa.read_with(v, |s, cx| s.host.read(cx).views());
        for t in views {
            t.update(v, |t, cx| t.test_term_feed(b"$ ", cx));
        }
        (f, sa, v)
    }

    fn tabs(sa: &Entity<SuperAgentView>, v: &mut VisualTestContext) -> Vec<ChatTab> {
        sa.read_with(v, |s, cx| {
            s.host
                .read(cx)
                .views()
                .iter()
                .map(|t| t.read(cx).pane_tab())
                .collect()
        })
    }

    fn shown(sa: &Entity<SuperAgentView>, v: &mut VisualTestContext) -> Option<Segment> {
        sa.read_with(v, |s, cx| s.side_shown(cx))
    }

    fn finish(
        f: &super::super::session::host::tests::Fx,
        sa: &Entity<SuperAgentView>,
        v: &mut VisualTestContext,
    ) {
        // the engines end with their frames
        while let Some(i) = sa.read_with(v, |s, _| {
            s.frames
                .tabs()
                .iter()
                .position(|f| matches!(f, Frame::Session(_)))
        }) {
            sa.update(v, |s, cx| s.close_tab(i, cx));
        }
        v.update(|_, _| ());
        v.run_until_parked();
        super::super::session::host::tests::wait_free(&f.home);
    }

    #[gpui::test]
    fn open_state_and_width_are_the_windows_and_the_panes_are_each_sessions(
        cx: &mut TestAppContext,
    ) {
        let (f, sa, v) = two(cx);
        assert_eq!(shown(&sa, v), None);
        assert!(press(v, &sa, "`", ctrl()));
        assert_eq!(shown(&sa, v), Some(Segment::Terminal));
        // only the front session (s2) gets the pane and with it the keys
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Terminal]);
        sa.update(v, |s, _| s.side.set_width(500., 1400.));
        // another session frame: the same open state, segment and width; the other session's pane
        sa.update(v, |s, cx| s.focus_tab(1, cx));
        v.run_until_parked();
        assert_eq!(tabs(&sa, v), [ChatTab::Terminal, ChatTab::Chat]);
        assert_eq!(
            sa.read_with(v, |s, _| (s.side.open, s.side.width)),
            (true, 500.)
        );
        // s2's shells are kept while its pane is hidden
        let kept = sa.read_with(v, |s, cx| {
            s.host.read(cx).views()[1].read(cx).term.is_some()
        });
        assert!(kept);
        // the home has no sidebar: nothing shows and no pane takes a key
        sa.update(v, |s, cx| s.focus_tab(0, cx));
        v.run_until_parked();
        assert_eq!(shown(&sa, v), None);
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Chat]);
        assert!(v.debug_bounds("sa-side").is_none());
        // back in a session: it is there again (a global choice)
        sa.update(v, |s, cx| s.focus_tab(2, cx));
        v.run_until_parked();
        assert_eq!(shown(&sa, v), Some(Segment::Terminal));
        assert!(v.debug_bounds("sa-side").is_some());
        // Browser, then Details (no pane for the Details segment), then the same icon closes
        sa.update(v, |s, cx| s.toggle_side(Segment::Browser, cx));
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Browser]);
        sa.update(v, |s, cx| s.toggle_side(Segment::Details, cx));
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Chat]);
        v.run_until_parked();
        assert!(v.debug_bounds("sa-details").is_some());
        sa.update(v, |s, cx| s.toggle_side(Segment::Details, cx));
        assert_eq!(shown(&sa, v), None);
        // the view is hidden (the TUI view on screen): no pane takes a key
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        sa.update(v, |s, cx| s.set_visible(false, cx));
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Chat]);
        sa.update(v, |s, cx| s.set_visible(true, cx));
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Terminal]);
        // closing the session frame ends its panes with it
        sa.update(v, |s, cx| s.close_tab(2, cx));
        v.run_until_parked();
        assert_eq!(sa.read_with(v, |s, cx| s.host.read(cx).ids()), ["s1"]);
        finish(&f, &sa, v);
    }

    #[gpui::test]
    fn the_title_bar_icons_follow_the_availability_table(cx: &mut TestAppContext) {
        let (f, sa, v) = two(cx);
        // a click on the Browser icon opens the sidebar on Browser
        let b = v.debug_bounds("sa-dock-browser").unwrap();
        v.simulate_click(b.center(), Modifiers::default());
        assert_eq!(shown(&sa, v), Some(Segment::Browser));
        // an outcome frame offers Details only: Terminal does nothing and says why
        sa.update(v, |s, cx| s.open(Frame::Outcome("o1".into()), cx));
        v.run_until_parked();
        assert_eq!(shown(&sa, v), Some(Segment::Details));
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        assert_eq!(sa.read_with(v, |s, _| s.side.seg), Segment::Browser);
        assert!(
            sa.read_with(v, |s, _| s.notice.clone())
                .unwrap()
                .contains("Terminal")
        );
        // the home: nothing at all, the icons are dimmed (no click target) and say why
        sa.update(v, |s, cx| s.focus_tab(0, cx));
        v.run_until_parked();
        sa.update(v, |s, cx| s.toggle_side(Segment::Details, cx));
        assert_eq!(shown(&sa, v), None);
        assert!(
            sa.read_with(v, |s, _| s.notice.clone())
                .unwrap()
                .contains("Details")
        );
        finish(&f, &sa, v);
    }

    #[gpui::test]
    fn closing_the_last_shell_closes_the_sidebar_and_reopening_starts_a_new_one(
        cx: &mut TestAppContext,
    ) {
        let (f, sa, v) = two(cx);
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        let front = sa.read_with(v, |s, cx| s.host.read(cx).views()[1].clone());
        front.update(v, |t, cx| t.test_term_feed(b"zsh$ ", cx));
        v.run_until_parked();
        assert_eq!(shown(&sa, v), Some(Segment::Terminal));
        front.update(v, |t, cx| t.test_term_close_all(cx));
        v.run_until_parked();
        assert!(!sa.read_with(v, |s, _| s.side.open));
        assert_eq!(tabs(&sa, v), [ChatTab::Chat, ChatTab::Chat]);
        // the next open of Terminal starts a shell again (a Failed or Starting tab at least), never an empty pane
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        v.run_until_parked();
        let n = front.read_with(v, |t, cx| t.term.as_ref().unwrap().read(cx).tabs().len());
        assert_eq!(n, 1);
        assert!(sa.read_with(v, |s, _| s.side.open));
        drop(front);
        finish(&f, &sa, v);
    }

    #[gpui::test]
    fn the_grip_resizes_with_the_pointer_and_with_left_and_right(cx: &mut TestAppContext) {
        let (f, sa, v) = two(cx);
        sa.update(v, |s, cx| s.toggle_side(Segment::Details, cx));
        v.run_until_parked();
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 340.);
        sa.update_in(v, |s, w, cx| s.drag_side(900., w, cx));
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 1400. - 8. - 900.);
        sa.update_in(v, |s, w, cx| s.drag_side(0., w, cx));
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 640.);
        sa.update_in(v, |s, w, cx| s.drag_side(1300., w, cx));
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 280.);
        // Left and Right on the focused grip
        sa.update_in(v, |s, w, cx| w.focus(&s.grip_focus.clone(), cx));
        assert!(press(v, &sa, "left", none()));
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 296.);
        assert!(press(v, &sa, "right", none()));
        assert_eq!(sa.read_with(v, |s, _| s.side.width), 280.);
        finish(&f, &sa, v);
    }

    /// The directory tree holds no copy of `needle` (the session store, transcripts, logs).
    fn home_has(dir: &std::path::Path, needle: &str) -> bool {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                let p = e.path();
                if p.is_dir() {
                    home_has(&p, needle)
                } else {
                    std::fs::read(&p)
                        .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                }
            })
    }

    /// What the conductor can read of this window's sessions, as one string.
    fn conductor_view(sa: &Entity<SuperAgentView>, v: &mut VisualTestContext) -> String {
        sa.update(v, |s, cx| s.refresh_port(cx));
        sa.read_with(v, |s, _| {
            let snap = s.port.shared.lock().unwrap();
            format!("{:?} {:?}", snap.rows, snap.reports)
        })
    }

    /// T15: what a person types or runs in the sidebar shell is never an event, a digest row or a transcript line.
    #[gpui::test]
    fn sa_term_not_in_digest(cx: &mut TestAppContext) {
        let (f, sa, v) = two(cx);
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        let front = sa.read_with(v, |s, cx| s.host.read(cx).views()[1].clone());
        let out = format!("$ echo {CANARY}\r\n{CANARY}\r\n");
        front.update(v, |t, cx| t.test_term_feed(out.as_bytes(), cx));
        v.run_until_parked();
        // it is on the person's screen ...
        let shells = front.read_with(v, |t, cx| t.term.as_ref().unwrap().read(cx).tabs().len());
        assert!(shells >= 1);
        // ... and nowhere the conductor or a model reads
        let seen = conductor_view(&sa, v);
        assert!(
            !seen.contains(CANARY) && !seen.contains("SIDEBARCANARY"),
            "{seen}"
        );
        assert!(
            !home_has(&f.home, "SIDEBARCANARY"),
            "a file under the synt home holds the output"
        );
        drop(front);
        finish(&f, &sa, v);
    }

    /// T15: page text and frames of the person's browser pane stay in the pane.
    #[gpui::test]
    fn sa_browser_frames_not_in_digest(cx: &mut TestAppContext) {
        let (f, sa, v) = two(cx);
        sa.update(v, |s, cx| s.toggle_side(Segment::Browser, cx));
        let front = sa.read_with(v, |s, cx| s.host.read(cx).views()[1].clone());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        front.update(v, |t, _| t.test_browser_page(CANARY, rt.handle()));
        let mut seen = (false, None);
        for _ in 0..300 {
            seen = front.read_with(v, |t, _| t.test_browser_seen());
            if seen.0 && seen.1.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(seen.0, "a frame reached the pane");
        assert_eq!(seen.1.as_deref(), Some(CANARY), "the page reached the pane");
        let view = conductor_view(&sa, v);
        assert!(!view.contains("SIDEBARCANARY"), "{view}");
        assert!(!home_has(&f.home, "SIDEBARCANARY"));
        drop(front);
        finish(&f, &sa, v);
    }

    /// Spec 5.3: OSC 52 is ignored in the sidebar shell, both ways; the clipboard is the person's.
    #[gpui::test]
    fn osc_52_from_the_sidebar_shell_never_touches_the_clipboard(cx: &mut TestAppContext) {
        let (f, sa, v) = two(cx);
        sa.update(v, |s, cx| s.toggle_side(Segment::Terminal, cx));
        let front = sa.read_with(v, |s, cx| s.host.read(cx).views()[1].clone());
        v.write_to_clipboard(ClipboardItem::new_string("keep".into()));
        // a store ("pwned"), then a load request
        front.update(v, |t, cx| {
            t.test_term_feed(b"\x1b]52;c;cHduZWQ=\x07\x1b]52;c;?\x07ok", cx)
        });
        v.run_until_parked();
        let got = v.read_from_clipboard().and_then(|c| c.text());
        assert_eq!(got.as_deref(), Some("keep"));
        drop(front);
        finish(&f, &sa, v);
    }

    /// T2: the sidebar shell is the TUI view's `TermPane` started by the same `launch::plan`; the plan drops every
    /// `SYNT_*` and secret-shaped name, and the sidebar has no spawn path of its own.
    #[test]
    fn t2_the_sidebar_shell_has_no_synt_or_secret_environment() {
        let env = synthesis_config::Env::from_pairs([
            ("PATH", "/usr/bin"),
            ("SYNT_HOME", "/h"),
            ("SYNT_API_KEY", "k"),
            ("OPENAI_API_KEY", "k"),
            ("GITHUB_TOKEN", "k"),
            ("AWS_SECRET_ACCESS_KEY", "k"),
        ]);
        let names: Vec<String> = crate::term::launch::scrub(&env, false)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(names.iter().any(|k| k == "PATH"));
        assert!(
            !names.iter().any(|k| k.starts_with("SYNT_")
                || k.contains("KEY")
                || k.contains("TOKEN")
                || k.contains("SECRET")),
            "{names:?}"
        );
        // the only place a shell starts is `term_launch`, through `launch::plan` (which scrubs)
        let pane = include_str!("../tui_view/term_pane.rs");
        assert_eq!(pane.matches("launch::plan(").count(), 1);
        let drv = include_str!("../tui_view/driver.rs");
        let embed =
            &drv[drv.find("fn embed_side").unwrap()..drv.find("fn embed_pane_closed").unwrap()];
        assert!(!embed.contains("spawn") && !embed.contains("Command"));
    }

    #[test]
    fn side_is_a_plain_window_state_with_the_spec_defaults() {
        let d = Side::default();
        assert_eq!((d.open, d.seg, d.width), (false, Segment::Details, 340.));
    }
}

// ------------------------------------------------------------------------------------------ C3: outcome and worker frames

fn outcomes_of(v: &mut VisualTestContext, sa: &Entity<SuperAgentView>) -> Entity<OutcomesView> {
    sa.read_with(v, |s, _| s.outcomes.clone())
}

#[gpui::test]
fn an_outcome_frame_lists_its_work_items_and_j_enter_opens_the_worker_frame(
    cx: &mut TestAppContext,
) {
    let (sa, v) = open(cx);
    sa.update(v, |s, cx| s.open(Frame::Outcome("o_auth".into()), cx));
    v.run_until_parked();
    assert!(
        v.debug_bounds("sa-outcome").is_some(),
        "the frame, not the placeholder"
    );
    assert!(v.debug_bounds("sa-frame").is_none());
    for row in [
        "oc-item-0",
        "oc-item-1",
        "oc-item-2",
        "oc-item-3",
        "oc-item-4",
    ] {
        assert!(v.debug_bounds(row).is_some(), "work item row {row}");
    }
    // the selected outcome is the one in front
    let o = outcomes_of(v, &sa);
    assert_eq!(
        o.read_with(v, |o, _| o.state.selected.clone()),
        Some("o_auth".into())
    );
    // the keys are on the frame: J moves the cursor, Enter opens the worker frame of that row
    v.simulate_keystrokes("j j");
    v.simulate_keystrokes("enter");
    v.run_until_parked();
    assert_eq!(active(v, &sa), Frame::Worker("o_auth".into(), "wi2".into()));
    assert!(v.debug_bounds("sa-worker").is_some());
    assert!(v.debug_bounds("wk-composer").is_some());
    // Escape leaves the frame's own focus first, then goes back to the outcome frame
    v.simulate_keystrokes("escape");
    assert!(matches!(active(v, &sa), Frame::Worker(..)));
    v.simulate_keystrokes("escape");
    assert_eq!(active(v, &sa), Frame::Outcome("o_auth".into()));
}

#[gpui::test]
fn a_pending_decision_is_answered_on_the_home_never_in_the_outcome_frame(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    sa.update(v, |s, cx| s.open(Frame::Outcome("o_auth".into()), cx));
    v.run_until_parked();
    assert!(v.debug_bounds("oc-needs").is_some());
    assert!(v.debug_bounds("home-send").is_none() && v.debug_bounds("home-opt-0").is_none());
    let b = v.debug_bounds("oc-answer").unwrap();
    v.simulate_click(b.center(), Modifiers::default());
    v.run_until_parked();
    assert_eq!(
        active(v, &sa),
        Frame::Home,
        "the home has the one decision card"
    );
}

#[gpui::test]
fn typing_in_a_worker_composer_is_not_a_shell_shortcut_and_a_frame_behind_gets_no_key(
    cx: &mut TestAppContext,
) {
    let (sa, v) = open(cx);
    let o = outcomes_of(v, &sa);
    o.update(v, |o, _| o.state.attempt_message = true);
    sa.update(v, |s, cx| {
        s.open(Frame::Worker("o_auth".into(), "wi3".into()), cx)
    });
    v.run_until_parked();
    // `n` is a shell shortcut on the home, but the composer in front takes it as text
    v.simulate_keystrokes("n");
    assert_eq!(cursor(v, &sa), None, "no picker opened");
    let w = sa.read_with(v, |s, _| s.worker_frames[0].1.clone());
    assert_eq!(w.read_with(v, |w, cx| w.draft(cx)), "n");
    // another frame in front: the worker frame behind takes nothing
    sa.update(v, |s, cx| s.focus_tab(0, cx));
    v.run_until_parked();
    v.simulate_keystrokes("x");
    assert_eq!(w.read_with(v, |w, cx| w.draft(cx)), "n");
}

#[gpui::test]
fn a_closed_worker_tab_keeps_its_draft_for_when_it_is_opened_again(cx: &mut TestAppContext) {
    let (sa, v) = open(cx);
    let o = outcomes_of(v, &sa);
    o.update(v, |o, _| o.state.attempt_message = true);
    let f = Frame::Worker("o_auth".into(), "wi3".into());
    sa.update(v, |s, cx| s.open(f.clone(), cx));
    v.run_until_parked();
    v.simulate_keystrokes("a b");
    sa.update(v, |s, cx| s.close_tab(1, cx));
    v.run_until_parked();
    assert_eq!(sa.read_with(v, |s, _| s.frames.draft(&f).to_string()), "ab");
    sa.update(v, |s, cx| s.open(f.clone(), cx));
    v.run_until_parked();
    let w = sa.read_with(v, |s, _| s.worker_frames[0].1.clone());
    assert_eq!(w.read_with(v, |w, cx| w.draft(cx)), "ab");
}
