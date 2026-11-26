//! Moving sessions between views with real engines on the scripted model: running, idle and awaiting-approval
//! sessions are swapped back and forth; none is dropped, doubled or cut off.

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, IntoElement, Render, TestAppContext, VisualTestContext,
    Window, px, size,
};
use gpui_component::input::InputEvent;
use synthesis_view::convo::Item;

use super::{Place, swap_live, where_is};
use crate::superagent::session::host::tests::{fx, wait_free};
use crate::tui_view::TuiView;

struct Root;

impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
    }
}

type Probe = (String, usize, (usize, usize), usize);

/// What must not change when a session moves: id, engine, approvals; plus what must be exact: tasks and
/// subscriptions (the pump and the clock once each; the hooks of the view it is in now).
fn probe(v: &mut VisualTestContext, view: &Entity<TuiView>) -> Probe {
    view.read_with(v, |t, _| {
        let lu = t.live.as_ref().unwrap();
        let e = lu.live.engine.as_ref().expect("the engine stays open");
        (
            lu.live.sid.clone(),
            Arc::as_ptr(e) as usize,
            lu.counts(),
            lu.live.dialogs.len(),
        )
    })
}

fn wait(v: &mut VisualTestContext, what: &str, ok: impl Fn(&mut VisualTestContext) -> bool) {
    for _ in 0..1200 {
        v.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        v.run_until_parked();
        if ok(v) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("not reached: {what}");
}

/// (user lines, assistant answers, a turn runs)
fn answers(v: &mut VisualTestContext, view: &Entity<TuiView>) -> (usize, usize, bool) {
    view.read_with(v, |t, _| {
        let c = t.live.as_ref().unwrap().live.convo();
        let n = |f: fn(&Item) -> bool| c.items.iter().filter(|i| f(i)).count();
        (
            n(|i| matches!(i, Item::User(_))),
            n(|i| matches!(i, Item::Assistant { .. })),
            t.embed_info().1,
        )
    })
}

#[gpui::test]
fn sessions_swap_between_views_without_losing_engine_approvals_tasks_or_events(
    cx: &mut TestAppContext,
) {
    let (f, cfg) = fx(3);
    std::fs::write(
        f.base.join("script.json"),
        r#"[{"text":"ok"},{"text":"ok"},{"text":"ok"}]"#,
    )
    .unwrap();
    cx.executor().allow_parking();
    cx.update(gpui_component::init);
    let mut views = vec![];
    let (_, v) = cx.add_window_view(|window, cx| {
        for _ in 0..3 {
            views.push(cx.new(|cx| TuiView::live(window, cx, cfg.clone(), Default::default(), 0.)));
        }
        Root
    });
    v.simulate_resize(size(px(1000.), px(700.)));
    let [a, b, t] = [views[0].clone(), views[1].clone(), views[2].clone()];
    let places = [
        (Place::Frame("a".into()), a.clone()),
        (Place::Frame("b".into()), b.clone()),
        (Place::Tui, t.clone()),
    ];
    a.update_in(v, |t, w, cx| {
        t.start_in(f.base.join("f0"), None, true, w, cx)
    })
    .unwrap();
    b.update_in(v, |t, w, cx| {
        t.start_in(f.base.join("f1"), None, true, w, cx)
    })
    .unwrap();
    t.update_in(v, |t, w, cx| t.start(w, cx));
    wait(v, "three engines", |v| {
        views.iter().all(|x| {
            x.read_with(v, |t, _| {
                t.live.as_ref().is_some_and(|l| l.live.engine.is_some())
            })
        })
    });
    // a runs a turn (its events arrive only after the swaps below), b is idle, t waits for an approval
    a.update_in(v, |t, w, cx| {
        let p = t.embed_prompt().unwrap();
        p.update(cx, |s, cx| s.set_value("hello", w, cx));
        t.embed_submit(w, cx)
    });
    t.update(v, |t, cx| t.push_test_question(cx));
    let base: Vec<Probe> = views.iter().map(|x| probe(v, x)).collect();
    assert_eq!(base[2].3, 1);
    let find = |v: &mut VisualTestContext, sid: &str| {
        let at = v
            .update(|_, cx| where_is(sid, &places, cx))
            .expect("a session is in exactly one place");
        places.iter().find(|(p, _)| *p == at).unwrap().1.clone()
    };

    for _round in 0..4 {
        for (x, y) in [(&a, &b), (&b, &t)] {
            v.update(|w, cx| swap_live(x, y, w, cx));
            for (sid, eng, _, dialogs) in &base {
                let view = find(v, sid);
                let (s2, e2, (tasks, subs), d2) = probe(v, &view);
                let embedded = view.read_with(v, |t, _| t.embed.is_some());
                assert_eq!(
                    (&s2, e2, d2),
                    (sid, *eng, *dialogs),
                    "session, engine, approvals"
                );
                assert_eq!(tasks, 2, "the pump and the clock, once each");
                assert_eq!(
                    subs,
                    3 + usize::from(embedded),
                    "the hooks of the view it is in"
                );
                assert!(
                    !synthesis_app::peers::session_lock_free(&f.home, sid),
                    "the lock stays held"
                );
            }
        }
    }
    // 4 rounds of a 3-cycle: a's session ends in the TUI view, not where it started
    assert!(find(v, &base[0].0) == t);
    // a's turn ran through all the swaps: its answer lands in whichever view shows it now
    let sid = base[0].0.clone();
    wait(v, "the turn's answer", |v| {
        let w = find(v, &sid);
        answers(v, &w) == (1, 1, false)
    });
    // the prompt box of the moved session sends from the view it is in now (not from the one it left)
    let w = find(v, &sid);
    w.update_in(v, |t, window, cx| {
        let p = t.embed_prompt().unwrap();
        p.update(cx, |s, cx| {
            s.set_value("again", window, cx);
            cx.emit(InputEvent::PressEnter {
                secondary: false,
                shift: false,
            })
        });
    });
    wait(v, "the second answer", |v| {
        let w = find(v, &sid);
        answers(v, &w) == (2, 2, false)
    });
    for other in &base[1..] {
        let w = find(v, &other.0);
        assert_eq!(answers(v, &w).0, 0, "nobody else was asked anything");
    }
    for x in &views {
        x.update(v, |t, cx| t.close_embedded(cx));
    }
    v.run_until_parked();
    wait_free(&f.home);
}
