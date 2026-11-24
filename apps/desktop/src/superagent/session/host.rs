//! Hosts the embedded chat of the session frames (phase C1, spec 5.3 and 5.6): one `TuiView` per open
//! `Frame::Session`, started in the frame's folder, kept while the tab lives, ended with it. The frame id is the
//! folder. Only the front frame, while the window is active, takes the keys.
//!
//! Wiring the owner (`SuperAgentView`, `Shell`) adds: `open` when a session folder arrives, `sync` whenever the
//! frames or the visible view change, `render_frame`/`info`/`needs_total`/`pane`/`set_side` where it draws.

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _,
    Styled as _, Subscription, Window, div,
};

use synthesis_app::conductor::SessionReport;
use synthesis_harness::PersonApproved;

use crate::superagent::frames::{Frame, Frames};
use crate::tui_view::driver::LiveCfg;
use crate::tui_view::{ChatTab, TuiView};

/// What the tab and the rail show of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub title: String,
    /// A turn runs (or the engine starts): the tab shows the running glyph, else idle.
    pub busy: bool,
    /// Permission asks and questions that wait for the person.
    pub needs: usize,
}

/// A hosted session asks to show its Terminal pane (a Run click in its chat): the frame's id.
pub struct WantTerminal(pub String);
impl gpui::EventEmitter<WantTerminal> for SessionHost {}

pub struct SessionHost {
    cfg: Option<LiveCfg>,
    views: Vec<(String, Entity<TuiView>)>,
    /// The session frame that is on screen (`None`: another frame, or another view of the window).
    front: Option<String>,
    window_active: bool,
    /// The front frame just changed: its prompt takes the focus when it is drawn.
    focus_pending: bool,
    _activation: Subscription,
}

/// First line of `s`, sanitized first and then cut to 60 characters (never the reverse: a cut escape sequence
/// would no longer be recognised).
fn tab_title(s: &str) -> String {
    let clean = synthesis_harness::sanitize_for_display(s.lines().next().unwrap_or(""));
    clean.chars().take(60).collect()
}

impl SessionHost {
    /// `cfg` `None` is the sample: no session is ever hosted.
    pub fn new(cfg: Option<LiveCfg>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe_window_activation(window, |h, window, cx| {
            h.window_active = window.is_window_active();
            h.apply(cx);
        });
        Self {
            cfg,
            views: Vec::new(),
            front: None,
            window_active: window.is_window_active(),
            focus_pending: false,
            _activation: sub,
        }
    }

    fn view(&self, id: &str) -> Option<&Entity<TuiView>> {
        self.views.iter().find(|(i, _)| i == id).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// Starts the session of frame `id` in `folder` (once: an open frame is kept). `Err(why)`: nothing is hosted
    /// (the sample, a refused folder), the owner keeps its placeholder and says why.
    pub fn open(
        &mut self,
        id: &str,
        folder: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.view(id).is_some() {
            return Ok(());
        }
        let Some(cfg) = self.cfg.clone() else {
            return Err("sessions need a live app".into());
        };
        let view = cx.new(|cx| TuiView::live(window, cx, cfg, Default::default(), 0.));
        let r = view.update(cx, |t, cx| {
            // keys only after `sync` says this frame is on screen
            (t.active, t.shown) = (false, false);
            t.start_in(folder.to_path_buf(), window, cx)
        });
        if r.is_err() {
            // a refused folder is no tab: it counts nowhere and nothing was started
            view.update(cx, |t, cx| t.close_embedded(cx));
            return r;
        }
        // the owner's tab, rail and conductor snapshot follow the chat
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        let fid = id.to_string();
        cx.subscribe(&view, move |_, _, _: &crate::tui_view::WantTerminal, cx| {
            cx.emit(WantTerminal(fid.clone()))
        })
        .detach();
        self.views.push((id.to_string(), view));
        Ok(())
    }

    /// The frames or the visible view changed: sessions whose tab is gone end (engine, shells, browser), the
    /// front session frame (when `visible`: the Super Agent view is the one on screen) gets the keys.
    pub fn sync(&mut self, frames: &Frames, visible: bool, cx: &mut Context<Self>) {
        let gone: Vec<_> = self
            .views
            .iter()
            .filter(|(i, _)| !frames.tabs().contains(&Frame::Session(i.clone())))
            .map(|(i, _)| i.clone())
            .collect();
        for id in gone {
            self.close(&id, cx);
        }
        let front = match frames.active() {
            Frame::Session(id) if visible && self.view(id).is_some() => Some(id.clone()),
            _ => None,
        };
        self.focus_pending |= front.is_some() && front != self.front;
        self.front = front;
        self.apply(cx);
    }

    /// Marks each view: shown and active only for the front frame of an active window.
    fn apply(&self, cx: &mut Context<Self>) {
        for (id, v) in &self.views {
            let on = self.front.as_deref() == Some(id);
            let act = on && self.window_active;
            v.update(cx, |t, cx| {
                if (t.shown, t.active) != (on, act) {
                    (t.shown, t.active) = (on, act);
                    cx.notify();
                }
            });
        }
        cx.notify();
    }

    /// Ends the session of frame `id`: its engine, its shells and its browser pane. The scratch folder it ran in
    /// stays (spec 5.6: never deleted by the app).
    pub fn close(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(i) = self.views.iter().position(|(v, _)| v == id) {
            let (_, view) = self.views.remove(i);
            // the closed view takes no key while it waits to be released
            view.update(cx, |t, cx| {
                (t.shown, t.active) = (false, false);
                t.close_embedded(cx)
            });
            if self.front.as_deref() == Some(id) {
                self.front = None;
            }
        }
    }

    /// The chat of frame `id` for the panel body. The first draw after the frame came to the front puts the
    /// keyboard focus in its prompt.
    pub fn render_frame(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view(id)?.clone();
        if self.focus_pending && self.front.as_deref() == Some(id) {
            self.focus_pending = false;
            let v = view.clone();
            window.defer(cx, move |window, cx| {
                v.update(cx, |t, cx| t.focus_prompt(window, cx))
            });
        }
        Some(div().size_full().child(view).into_any_element())
    }

    /// Title and state of the tab: the first thing the person asked, else the folder's name.
    pub fn info(&self, id: &str, cx: &App) -> Option<Info> {
        let (first, busy, needs) = self.view(id)?.read(cx).embed_info();
        let clean = tab_title;
        let title = first.map(|f| clean(&f)).filter(|t| !t.trim().is_empty());
        let folder = || {
            let f = self
                .view(id)
                .and_then(|v| v.read(cx).embed.as_ref().and_then(|e| e.folder.clone()));
            f.as_deref()
                .unwrap_or(std::path::Path::new(id))
                .file_name()
                .map_or_else(|| id.to_string(), |n| clean(&n.to_string_lossy()))
        };
        Some(Info {
            title: title.unwrap_or_else(folder),
            busy,
            needs,
        })
    }

    /// Questions waiting in every hosted session: the rail adds them to its needs-you count.
    pub fn needs_total(&self, cx: &App) -> usize {
        self.views
            .iter()
            .map(|(_, v)| v.read(cx).embed_info().2)
            .sum()
    }

    /// The hosted views (tests of the owner).
    #[cfg(test)]
    pub(crate) fn views(&self) -> Vec<Entity<TuiView>> {
        self.views.iter().map(|(_, v)| v.clone()).collect()
    }

    /// Every hosted session that has a store id, with `last` (a frame id) as the focused one. Engines that
    /// have not reported a session yet are left out (nothing stored to list).
    pub fn handoff(&self, last: Option<&str>, cx: &App) -> crate::handoff::Handoff {
        let mut h = crate::handoff::Handoff::default();
        for (id, v) in &self.views {
            if let Some(s) = v.read(cx).embed_handoff() {
                if last == Some(id.as_str()) {
                    h.focused = Some((s.scope.clone(), s.sid.clone()));
                }
                h.sessions.push(s);
            }
        }
        h
    }

    /// The ids of the hosted sessions, oldest first.
    pub fn ids(&self) -> Vec<String> {
        self.views.iter().map(|(i, _)| i.clone()).collect()
    }

    /// What the conductor may read of session `id`: its engine's session id and the report.
    pub fn read(&self, id: &str, cx: &App) -> Option<(String, SessionReport)> {
        Some(self.view(id)?.read(cx).embed_read())
    }

    /// The conductor's approved line to the running task of session `id`.
    pub fn send(
        &self,
        id: &str,
        text: String,
        approval: PersonApproved,
        cx: &App,
    ) -> Result<(), String> {
        let v = self.view(id).ok_or("that session is closed")?;
        v.read(cx).embed_send_via_conductor(text, approval)
    }

    pub fn interrupt(&self, id: &str, cx: &App) -> Result<(), String> {
        self.view(id)
            .ok_or("that session is closed")?
            .read(cx)
            .embed_interrupt();
        Ok(())
    }

    /// The first message of a session the conductor started (the person approved it): sent once the engine is up.
    pub fn first_message(
        &mut self,
        id: &str,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(v) = self.view(id).cloned() {
            v.update(cx, |t, cx| t.embed_first_message(text, window, cx));
        }
    }

    /// Which pane of session `id` the sidebar shows (`None`: none).
    pub fn set_side(&mut self, id: &str, side: Option<ChatTab>, cx: &mut Context<Self>) {
        if let Some(v) = self.view(id) {
            v.update(cx, |t, cx| t.embed_side(side, cx));
        }
    }

    /// Which pane of which session the sidebar shows: `front` gets it, every other session none (its shells keep
    /// running, its browser stops casting).
    pub fn set_sides(&mut self, front: Option<(&str, ChatTab)>, cx: &mut Context<Self>) {
        for (id, v) in &self.views {
            let side = front.filter(|(f, _)| f == id).map(|(_, k)| k);
            v.update(cx, |t, cx| t.embed_side(side, cx));
        }
    }

    /// The sidebar pane of session `id` lost its last inner tab.
    pub fn pane_closed(&self, id: &str, kind: ChatTab, cx: &App) -> bool {
        self.view(id)
            .is_some_and(|v| v.read(cx).embed_pane_closed(kind, cx))
    }

    /// Session `id` has no browser (WSL mode).
    pub fn remote(&self, id: &str, cx: &App) -> bool {
        self.view(id).is_some_and(|v| v.read(cx).embed_remote())
    }

    /// The Terminal or Browser pane of session `id`, for the right sidebar.
    pub fn pane(&mut self, id: &str, kind: ChatTab, cx: &mut Context<Self>) -> Option<AnyElement> {
        let v = self.view(id)?.clone();
        Some(v.update(cx, |t, cx| t.embed_pane(kind, cx)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::Args;
    use clap::Parser as _;
    use gpui::{Render, TestAppContext, VisualTestContext, px, size};
    use std::path::{Path, PathBuf};

    /// Draws the front session the way the owner's panel body does.
    struct Root(Entity<SessionHost>);

    impl Render for Root {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let id = self.0.read(cx).front.clone();
            let body = id.and_then(|id| self.0.update(cx, |h, cx| h.render_frame(&id, window, cx)));
            div().size_full().children(body)
        }
    }

    pub(crate) struct Fx {
        _t: tempfile::TempDir,
        pub(crate) home: PathBuf,
        pub(crate) base: PathBuf,
    }

    /// A scripted-model config on a temp home; `n` folders `f0..` to start sessions in.
    pub(crate) fn fx(n: usize) -> (Fx, LiveCfg) {
        let t = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(t.path()).unwrap();
        let home = base.join("home");
        for i in 0..n.max(1) {
            std::fs::create_dir_all(base.join(format!("f{i}"))).unwrap();
        }
        let script = base.join("script.json");
        std::fs::write(&script, r#"[{"text":"ok"}]"#).unwrap();
        let provider = format!("fake:{}", script.display());
        let cfg = LiveCfg {
            env: synthesis_config::Env::from_pairs([
                ("HOME", home.to_str().unwrap()),
                ("SYNT_HOME", home.to_str().unwrap()),
            ]),
            args: Args::try_parse_from([
                "synthesis-desktop",
                "--provider",
                &provider,
                "--sandbox",
                "off",
            ])
            .unwrap(),
            granted: base.join("f0"),
            project: base.join("f0"),
            synthesis_home: home.clone(),
            home: None,
            tz_min: 0,
            tui: Default::default(),
        };
        (Fx { _t: t, home, base }, cfg)
    }

    fn host(
        cx: &mut TestAppContext,
        cfg: Option<LiveCfg>,
    ) -> (Entity<SessionHost>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        let mut h = None;
        let (_, v) = cx.add_window_view(|window, cx| {
            let host = cx.new(|cx| SessionHost::new(cfg, window, cx));
            cx.observe(&host, |_, _, cx| cx.notify()).detach();
            h = Some(host);
            Root(h.clone().unwrap())
        });
        v.simulate_resize(size(px(1000.), px(700.)));
        (h.unwrap(), v)
    }

    fn open(h: &Entity<SessionHost>, v: &mut VisualTestContext, p: &Path) -> bool {
        try_open(h, v, p).is_ok()
    }

    fn try_open(
        h: &Entity<SessionHost>,
        v: &mut VisualTestContext,
        p: &Path,
    ) -> Result<(), String> {
        let id = p.to_string_lossy().into_owned();
        h.update_in(v, |h, w, cx| h.open(&id, p, w, cx))
    }

    fn frames(ids: &[&Path], front: Option<&Path>) -> Frames {
        let mut f = Frames::default();
        for p in ids.iter().chain(front.iter()) {
            f.open(Frame::Session(p.to_string_lossy().into_owned()));
        }
        f
    }

    fn flags(h: &Entity<SessionHost>, v: &mut VisualTestContext) -> Vec<(bool, bool)> {
        h.read_with(v, |h, cx| {
            h.views
                .iter()
                .map(|(_, t)| t.read_with(cx, |t, _| (t.shown, t.active)))
                .collect()
        })
    }

    /// The engine thread cannot wake the test scheduler: every engine the tests start must be gone (its
    /// session lock released) once its view is closed, whichever side of the channel the result is on.
    fn locks_all_free(home: &Path) -> bool {
        let dir = home.join("sessions");
        let ids: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        !ids.is_empty()
            && ids
                .iter()
                .all(|i| synthesis_app::peers::session_lock_free(home, i))
    }

    #[gpui::test]
    fn each_session_frame_gets_its_own_embedded_chat_and_only_the_front_one_takes_keys(
        cx: &mut TestAppContext,
    ) {
        let (f, cfg) = fx(3);
        cx.executor().allow_parking();
        let (a, b) = (f.base.join("f0"), f.base.join("f1"));
        let (h, v) = host(cx, Some(cfg));
        assert!(open(&h, v, &a) && open(&h, v, &b));
        assert!(open(&h, v, &a), "an open frame is kept, not started twice");
        assert_eq!(h.read_with(v, |h, _| h.len()), 2);
        h.read_with(v, |h, cx| {
            let (x, y) = (&h.views[0].1, &h.views[1].1);
            assert_ne!(
                x.entity_id(),
                y.entity_id(),
                "no shared view, engine or PTY"
            );
            assert!(x.read(cx).embed.is_some() && y.read(cx).embed.is_some());
        });
        assert_eq!(
            flags(&h, v),
            [(false, false); 2],
            "no keys before the owner syncs"
        );
        // B is the front frame of an active window (a test window starts inactive)
        h.update(v, |h, _| h.window_active = true);
        let fr = frames(&[&a], Some(&b));
        h.update(v, |h, cx| h.sync(&fr, true, cx));
        assert_eq!(flags(&h, v), [(false, false), (true, true)]);
        // another view of the window (Settings, the TUI view): nobody takes keys
        h.update(v, |h, cx| h.sync(&fr, false, cx));
        assert_eq!(flags(&h, v), [(false, false); 2]);
        // the window loses activation: shown, but no keys
        h.update(v, |h, cx| h.sync(&fr, true, cx));
        h.update(v, |h, cx| {
            h.window_active = false;
            h.apply(cx)
        });
        assert_eq!(flags(&h, v), [(false, false), (true, false)]);
        // a frame that is not a session in front
        let home = Frames::default();
        h.update(v, |h, cx| h.sync(&home, true, cx));
        assert_eq!(
            h.read_with(v, |h, _| h.len()),
            0,
            "sessions whose tabs are gone end"
        );
        let (_, w) = (f.base.clone(), v);
        w.run_until_parked();
        wait_free(&f.home);
    }

    pub(crate) fn wait_free(home: &Path) {
        // a slow runner (Windows) may need long to tear an engine down: the bound is generous, the assertion unchanged
        for _ in 0..2400 {
            if locks_all_free(home) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("an engine of a closed session is still alive");
    }

    #[gpui::test]
    fn the_front_session_draws_a_real_chat_and_gets_the_prompt_focus(cx: &mut TestAppContext) {
        let (f, cfg) = fx(2);
        cx.executor().allow_parking();
        let (a, b) = (f.base.join("f0"), f.base.join("f1"));
        let (h, v) = host(cx, Some(cfg));
        assert!(open(&h, v, &a) && open(&h, v, &b));
        v.run_until_parked();
        assert!(
            v.debug_bounds("view").is_none(),
            "no session is in front yet"
        );
        h.update(v, |h, _| h.window_active = true);
        let fr = frames(&[&a], Some(&b));
        h.update(v, |h, cx| h.sync(&fr, true, cx));
        v.run_until_parked();
        assert!(
            v.debug_bounds("view").is_some(),
            "the embedded chat is drawn"
        );
        assert!(
            !h.read_with(v, |h, _| h.focus_pending),
            "the prompt took the focus"
        );
        h.update(v, |h, cx| h.sync(&Frames::default(), true, cx));
        v.run_until_parked();
        wait_free(&f.home);
    }

    #[gpui::test]
    fn more_than_six_sessions_are_hosted(cx: &mut TestAppContext) {
        let (f, cfg) = fx(12);
        cx.executor().allow_parking();
        let (h, v) = host(cx, Some(cfg));
        for i in 0..12 {
            assert!(open(&h, v, &f.base.join(format!("f{i}"))));
        }
        assert_eq!(h.read_with(v, |h, _| h.len()), 12);
        h.update(v, |h, cx| h.sync(&Frames::default(), true, cx));
        v.run_until_parked();
        wait_free(&f.home);
    }

    #[gpui::test]
    fn the_sample_hosts_nothing_and_a_bad_folder_leaves_a_note_not_an_engine(
        cx: &mut TestAppContext,
    ) {
        let (h, v) = host(cx, None);
        assert!(!open(&h, v, Path::new("/")));
        let (f, cfg) = fx(7);
        cx.executor().allow_parking();
        let (h, v) = host(cx, Some(cfg));
        // a refused folder is no tab: it is not hosted
        let why = try_open(&h, v, &f.base.join("missing")).unwrap_err();
        assert!(why.starts_with("no such folder"), "{why}");
        assert_eq!(h.read_with(v, |h, _| h.len()), 0);
        h.update(v, |h, cx| h.sync(&Frames::default(), true, cx));
        v.run_until_parked();
        wait_free(&f.home);
    }

    /// A closed view keeps no key intercept alive while it waits to be released.
    #[gpui::test]
    fn a_closed_session_takes_no_key_even_while_the_entity_lives(cx: &mut TestAppContext) {
        let (f, cfg) = fx(1);
        cx.executor().allow_parking();
        let a = f.base.join("f0");
        let (h, v) = host(cx, Some(cfg));
        assert!(open(&h, v, &a));
        h.update(v, |h, _| h.window_active = true);
        h.update(v, |h, cx| h.sync(&frames(&[], Some(&a)), true, cx));
        let kept = h.read_with(v, |h, _| h.views[0].1.clone());
        assert_eq!(kept.read_with(v, |t, _| (t.shown, t.active)), (true, true));
        let id = a.to_string_lossy().into_owned();
        h.update(v, |h, cx| h.close(&id, cx));
        assert_eq!(
            kept.read_with(v, |t, _| (t.shown, t.active)),
            (false, false)
        );
        drop(kept);
        v.update(|_, _| ()); // the release is processed with the effects
        v.run_until_parked();
        wait_free(&f.home);
    }

    #[test]
    fn a_title_is_sanitized_before_it_is_cut() {
        assert_eq!(tab_title("fix the bug\nsecond line"), "fix the bug");
        let t = tab_title(&format!("{}\u{1b}[31mred", "a".repeat(58)));
        assert!(t.chars().count() <= 60 && !t.contains('\u{1b}'), "{t:?}");
    }

    /// Closing a tab ends the chat but never deletes the folder it ran in (a scratch folder is the person's).
    #[gpui::test]
    fn closing_a_session_keeps_its_scratch_folder_and_the_title_falls_back_to_the_folder(
        cx: &mut TestAppContext,
    ) {
        let (f, cfg) = fx(1);
        cx.executor().allow_parking();
        let scratch = crate::outcomes_view::kind_picker::make_scratch(&f.home).unwrap();
        let (h, v) = host(cx, Some(cfg));
        assert!(open(&h, v, &scratch));
        let id = scratch.to_string_lossy().into_owned();
        let info = h.read_with(v, |h, cx| h.info(&id, cx)).unwrap();
        assert_eq!(
            info.title,
            scratch.file_name().unwrap().to_string_lossy(),
            "no prompt yet: the folder's name"
        );
        h.update(v, |h, cx| h.close(&id, cx));
        assert!(scratch.is_dir());
        v.run_until_parked();
        wait_free(&f.home);
    }
}
