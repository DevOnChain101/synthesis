//! The Terminal tab's personal shells (docs/specs/terminal-browser.md 1.10): a GPUI grid that draws the cell
//! grid of a `term::session::Session`, keys, mouse, selection, clipboard, IME, scrollback and the strip of
//! shell tabs. The shell is the person's own: unconfined, invisible to the agent, never an event or a log.
//!
//! - Painting copies the visible cells under the lock, then draws backgrounds as quads and each row as one
//!   shaped line with a forced cell width (GPUI caches shaped lines, so an unchanged row costs a lookup);
//!   repaints are capped at 60 per second and driven by the reader thread's coalesced wake-up.
//! - shortcut: a wide character is squeezed into one cell's advance (its spacer cell is skipped) and
//!   combining marks are dropped; upgrade to per-glyph placement if CJK output matters.

use std::cell::Cell as StdCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::CursorShape;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, AsyncApp, Bounds, ClipboardItem, Context, Div, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollEvent, StatefulInteractiveElement as _, Styled as _, TextAlign,
    TextRun, UTF16Selection, UnderlineStyle, WeakEntity, Window, canvas, div, point, px, size,
};
use synthesis_cli_ui_render::theme as t;

use super::TuiView;
use crate::brand;
use crate::term::keys::{self, Key, Mods};
use crate::term::launch::{self, Launch, Where};
use crate::term::session::{DEFAULT_BG, Session, Snap};
use crate::theme::hsla;

const FONT_PX: f32 = 13.;
const LINE_H: f32 = 17.;
const PAD: f32 = 6.;
/// Personal shells per pane (spec 1.7).
pub const MAX_SHELLS: usize = 8;

/// What the strip needs to know changed (a title, an exit, a shell added or removed).
pub struct Changed;

enum State {
    Starting,
    Running(Arc<Session>),
    /// The shell could not start (the reason is shown, with a retry).
    Failed(String),
}

struct Shell {
    n: u32,
    state: State,
    label: String,
    /// What the strip showed last: a change sends [`Changed`].
    seen: (String, bool),
}

/// Where the grid is on screen, for the pointer and the input method.
#[derive(Clone, Copy, Default)]
struct Geo {
    origin: Point<Pixels>,
    cw: f32,
    ch: f32,
    cols: usize,
    rows: usize,
    cursor: (usize, usize),
}

/// What a keystroke did, for the window-wide key hook.
pub enum KeyOutcome {
    /// Taken (sent to the shell or an action of the pane).
    Taken,
    /// Plain text: the platform's input method commits it through the input handler.
    Ime,
    /// Not the terminal's: an app shortcut.
    Pass,
}

pub struct TermPane {
    shells: Vec<Shell>,
    cur: usize,
    next_n: u32,
    /// The strip shows the agent's command log instead of a shell.
    pub agent: bool,
    focus: FocusHandle,
    want_focus: bool,
    geo: Rc<StdCell<Geo>>,
    marked: Option<String>,
    drag: bool,
    scroll_acc: f32,
    /// A one-line problem (a paste too large, the shell not reading) and the link waiting for a yes.
    notice: Option<String>,
    link: Option<String>,
    /// A close of a shell with a program running waits for a second click.
    confirm_close: Option<u32>,
    /// The pane tried to open its first shell (it does not retry by itself).
    pub tried: bool,
    /// Size handed to a new shell: the last drawn grid.
    last_size: (u16, u16),
}

impl EventEmitter<Changed> for TermPane {}

impl Focusable for TermPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn mods(m: &gpui::Modifiers) -> Mods {
    Mods {
        ctrl: m.control,
        alt: m.alt,
        shift: m.shift,
        platform: m.platform,
    }
}

/// The part of an http(s) link that may be shown and opened: one line, no controls, clipped.
fn link_ok(uri: &str) -> Option<String> {
    let u = uri.trim();
    ((u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 2048
        && !u.chars().any(|c| c.is_control() || c.is_whitespace()))
    .then(|| u.to_string())
}

impl TermPane {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            shells: vec![],
            cur: 0,
            next_n: 1,
            agent: false,
            focus: cx.focus_handle(),
            want_focus: false,
            geo: Rc::default(),
            marked: None,
            drag: false,
            scroll_acc: 0.,
            notice: None,
            link: None,
            confirm_close: None,
            tried: false,
            last_size: (80, 24),
        }
    }

    pub fn focus(&mut self, cx: &mut Context<Self>) {
        self.want_focus = true;
        cx.notify();
    }

    /// Columns and rows of the grid as last drawn.
    #[cfg(test)]
    pub fn grid_size(&self) -> Option<(usize, usize)> {
        let g = self.geo.get();
        (g.cols > 0).then_some((g.cols, g.rows))
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// `(n, label, selected)` for the strip.
    pub fn tabs(&self) -> Vec<(u32, String, bool)> {
        self.shells
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let title = match &s.state {
                    State::Running(x) => x.title(),
                    _ => String::new(),
                };
                let label = if title.is_empty() {
                    format!("{} {}", s.n, s.label)
                } else {
                    format!("{} {}", s.n, title)
                };
                (s.n, label, !self.agent && i == self.cur)
            })
            .collect()
    }

    pub fn select(&mut self, n: u32, cx: &mut Context<Self>) {
        if let Some(i) = self.shells.iter().position(|s| s.n == n) {
            (self.cur, self.agent) = (i, false);
            self.focus(cx);
        }
    }

    fn session(&self) -> Option<Arc<Session>> {
        match &self.shells.get(self.cur)?.state {
            State::Running(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Opens a shell from `plan` (the launch or why there is none). The spawn runs off the UI thread.
    pub fn open(&mut self, plan: Result<Launch, String>, cx: &mut Context<Self>) {
        self.tried = true;
        if self.shells.len() >= MAX_SHELLS {
            self.notice = Some(format!(
                "At most {MAX_SHELLS} shells are open; close one first."
            ));
            cx.notify();
            return;
        }
        let n = self.next_n;
        self.next_n += 1;
        let label = plan
            .as_ref()
            .map_or_else(|_| "shell".into(), |l| l.label.clone());
        let state = match &plan {
            Ok(_) => State::Starting,
            Err(why) => State::Failed(why.clone()),
        };
        self.shells.push(Shell {
            n,
            state,
            label,
            seen: Default::default(),
        });
        (self.cur, self.agent, self.notice) = (self.shells.len() - 1, false, None);
        self.want_focus = true;
        cx.emit(Changed);
        cx.notify();
        let Ok(l) = plan else { return };
        let (cols, rows) = self.last_size;
        let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.unbounded_send(());
        });
        let spawn = cx
            .background_executor()
            .spawn(async move { Session::spawn(&l, cols, rows, wake) });
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let r = spawn.await.map(Arc::new);
            let Some(s) = this.update(cx, |p, cx| p.started(n, r, cx)).ok().flatten() else {
                return;
            };
            let mut rx = rx;
            while rx.next().await.is_some() {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let alive = this.update(cx, |p, cx| {
                    s.dirty.store(false, Ordering::Release);
                    p.after_output(n, cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// The spawn finished: the session to follow, or the reason it failed.
    fn started(
        &mut self,
        n: u32,
        r: Result<Arc<Session>, String>,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Session>> {
        // the shell was closed while it started: the session drops here and kills itself
        let sh = self.shells.iter_mut().find(|s| s.n == n)?;
        match r {
            Ok(s) => {
                sh.state = State::Running(s.clone());
                cx.emit(Changed);
                cx.notify();
                Some(s)
            }
            Err(why) => {
                sh.state = State::Failed(why);
                cx.notify();
                None
            }
        }
    }

    fn after_output(&mut self, n: u32, cx: &mut Context<Self>) {
        cx.notify();
        let Some(sh) = self.shells.iter_mut().find(|s| s.n == n) else {
            return;
        };
        if let State::Running(s) = &sh.state {
            let now = (s.title(), s.exited().is_some());
            if now != sh.seen {
                sh.seen = now;
                cx.emit(Changed);
            }
        }
    }

    /// A screen fed with a fixed byte script and no process (`--shot --pty-script`).
    #[cfg_attr(not(feature = "shot"), allow(dead_code))]
    pub fn install_script(&mut self, bytes: &[u8], cols: u16, rows: u16, cx: &mut Context<Self>) {
        let n = self.next_n;
        self.next_n += 1;
        let s = Arc::new(Session::scripted(cols, rows, bytes, "zsh"));
        self.shells.push(Shell {
            n,
            state: State::Running(s),
            label: "zsh".into(),
            seen: Default::default(),
        });
        (self.cur, self.tried, self.agent) = (self.shells.len() - 1, true, false);
        cx.emit(Changed);
        cx.notify();
    }

    /// Closes shell `n`; one with a program running asks for a second click first.
    pub fn close(&mut self, n: u32, cx: &mut Context<Self>) {
        let Some(i) = self.shells.iter().position(|s| s.n == n) else {
            return;
        };
        if let State::Running(s) = &self.shells[i].state
            && s.exited().is_none()
            && s.busy()
            && self.confirm_close != Some(n)
        {
            self.confirm_close = Some(n);
            self.notice =
                Some("A program is running in this shell. Click x again to close it.".into());
            cx.notify();
            return;
        }
        self.shells.remove(i);
        self.cur = self.cur.min(self.shells.len().saturating_sub(1));
        (self.confirm_close, self.notice) = (None, None);
        cx.emit(Changed);
        cx.notify();
    }

    /// Replaces the current (ended or failed) shell by a new one in its place.
    pub fn restart(&mut self, plan: Result<Launch, String>, cx: &mut Context<Self>) {
        let at = self.cur;
        if at < self.shells.len() {
            self.shells.remove(at);
        }
        self.open(plan, cx); // appends (there is room: one was just removed)
        if let Some(l) = self.shells.pop() {
            let i = at.min(self.shells.len());
            self.shells.insert(i, l);
            self.cur = i;
        }
    }

    /// Whether the current shell ended or failed (Enter restarts it).
    pub fn needs_restart(&self) -> bool {
        self.shells.get(self.cur).is_some_and(|s| match &s.state {
            State::Failed(_) => true,
            State::Running(x) => x.exited().is_some(),
            State::Starting => false,
        })
    }

    fn send(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(s) = self.session() else { return };
        s.select_clear();
        self.notice = s.send(bytes).err();
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.session().and_then(|s| s.selected_text()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.session() else { return };
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        if text.len() > keys::MAX_PASTE {
            self.notice = Some(format!(
                "The clipboard holds more than {} KiB; not pasted.",
                keys::MAX_PASTE >> 10
            ));
            cx.notify();
            return;
        }
        let bytes = keys::paste(&text, s.mode());
        self.send(&bytes, cx);
    }

    /// A keystroke while the pane has the keys (see [`KeyOutcome`]). App chords are the caller's.
    pub fn key(&mut self, ks: &Keystroke, cx: &mut Context<Self>) -> KeyOutcome {
        let mac = cfg!(target_os = "macos");
        let (key, m) = (ks.key.as_str(), &ks.modifiers);
        if (m.control && !m.alt && !m.shift && matches!(key, "1" | "2" | "3" | "4" | "5"))
            || (m.control && key == "escape")
        {
            return KeyOutcome::Pass;
        }
        // copy, paste, select all, history: Cmd on the mac, Ctrl+Shift elsewhere (Ctrl+C is the shell's)
        let chord = if mac {
            m.platform && !m.shift
        } else {
            m.control && m.shift
        };
        match key {
            "c" if chord => {
                self.copy(cx);
                return KeyOutcome::Taken;
            }
            "v" if chord => {
                self.paste(cx);
                return KeyOutcome::Taken;
            }
            "insert" if m.shift && !m.control && !m.alt => {
                self.paste(cx);
                return KeyOutcome::Taken;
            }
            "a" if chord => {
                if let Some(s) = self.session() {
                    s.select_all();
                    cx.notify();
                }
                return KeyOutcome::Taken;
            }
            "pageup" | "pagedown" | "home" | "end" if m.shift && !m.control && !m.alt => {
                if let Some(s) = self.session() {
                    let page = i32::try_from(self.geo.get().rows).unwrap_or(24).max(2) - 1;
                    match key {
                        "pageup" => s.scroll(page),
                        "pagedown" => s.scroll(-page),
                        "home" => s.scroll(i32::MAX / 2),
                        _ => s.scroll(i32::MIN / 2),
                    }
                    cx.notify();
                }
                return KeyOutcome::Taken;
            }
            _ => {}
        }
        if self.needs_restart() {
            // the shell is gone: Enter starts a new one (the caller knows where), anything else is ignored
            return if key == "enter" {
                KeyOutcome::Pass
            } else {
                KeyOutcome::Taken
            };
        }
        let Some(s) = self.session() else {
            return KeyOutcome::Pass;
        };
        match keys::encode(key, ks.key_char.as_deref(), mods(m), s.mode(), mac) {
            Key::Bytes(b) => {
                self.send(&b, cx);
                KeyOutcome::Taken
            }
            Key::Text => KeyOutcome::Ime,
            Key::Pass => KeyOutcome::Pass,
        }
    }

    // ---- pointer ----

    /// The cell under a window point and whether the point is in its right half.
    fn cell_at(&self, p: Point<Pixels>) -> (usize, usize, bool) {
        let g = self.geo.get();
        let (x, y) = (
            f32::from(p.x - g.origin.x) / g.cw.max(1.),
            f32::from(p.y - g.origin.y) / g.ch.max(1.),
        );
        let col = (x.floor().max(0.) as usize).min(g.cols.saturating_sub(1));
        let row = (y.floor().max(0.) as usize).min(g.rows.saturating_sub(1));
        (col, row, x - x.floor() >= 0.5)
    }

    fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(s) = self.session() else { return };
        let (col, row, right) = self.cell_at(e.position);
        let mode = s.mode();
        let button = match e.button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        if mode.intersects(TermMode::MOUSE_MODE) && !e.modifiers.shift {
            if let Some(b) = keys::mouse(button, true, false, col, row, mods(&e.modifiers), mode) {
                self.send(&b, cx);
            }
            self.drag = true;
            return;
        }
        if e.button != MouseButton::Left {
            return;
        }
        let ctrl_click = if cfg!(target_os = "macos") {
            e.modifiers.platform
        } else {
            e.modifiers.control
        };
        if ctrl_click {
            // a link opens only after the person confirms the URL shown
            if let Some(u) = s.link_at(col, row) {
                match link_ok(&u) {
                    Some(u) => self.link = Some(u),
                    None => {
                        self.notice =
                            Some("Only http and https links open from the terminal.".into());
                    }
                }
            }
            cx.notify();
            return;
        }
        let ty = match e.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        s.select_start(ty, col, row, right);
        self.drag = true;
        cx.notify();
    }

    fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.drag {
            return;
        }
        let Some(s) = self.session() else { return };
        let (col, row, right) = self.cell_at(e.position);
        let mode = s.mode();
        if mode.intersects(TermMode::MOUSE_MODE) && !e.modifiers.shift {
            if mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
                && let Some(b) = keys::mouse(0, true, true, col, row, mods(&e.modifiers), mode)
            {
                self.send(&b, cx);
            }
            return;
        }
        s.select_to(col, row, right);
        cx.notify();
    }

    fn mouse_up(&mut self, e: &MouseUpEvent, cx: &mut Context<Self>) {
        let was = std::mem::take(&mut self.drag);
        let Some(s) = self.session() else { return };
        let mode = s.mode();
        if was && mode.intersects(TermMode::MOUSE_MODE) && !e.modifiers.shift {
            let (col, row, _) = self.cell_at(e.position);
            if let Some(b) = keys::mouse(0, false, false, col, row, mods(&e.modifiers), mode) {
                self.send(&b, cx);
            }
        }
    }

    fn wheel(&mut self, e: &ScrollEvent, cx: &mut Context<Self>) {
        let Some(s) = self.session() else { return };
        let g = self.geo.get();
        self.scroll_acc += f32::from(e.delta.pixel_delta(px(g.ch.max(1.))).y) / g.ch.max(1.);
        let lines = self.scroll_acc.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_acc -= lines as f32;
        let mode = s.mode();
        let up = lines > 0;
        if mode.intersects(TermMode::MOUSE_MODE) && !e.modifiers.shift {
            let (col, row, _) = self.cell_at(e.position);
            if let Some(b) = keys::mouse(
                if up { 64 } else { 65 },
                true,
                false,
                col,
                row,
                mods(&e.modifiers),
                mode,
            ) {
                for _ in 0..lines.unsigned_abs().min(10) {
                    self.send(&b, cx);
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
            let arrow: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.send(&arrow.repeat(lines.unsigned_abs().min(10) as usize), cx);
        } else {
            s.scroll(lines);
            cx.notify();
        }
    }
}

// ---- the input method (IME): preedit at the cursor, commit as UTF-8 ----

impl EntityInputHandler for TermPane {
    fn text_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.marked.as_ref().map(|m| 0..m.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        // the text of a commit is the person's typing: one write, no control characters
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if !clean.is_empty() {
            self.send(clean.as_bytes(), cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        new_text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = Some(new_text.chars().filter(|c| !c.is_control()).collect())
            .filter(|m: &String| !m.is_empty());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let g = self.geo.get();
        let (c, r) = g.cursor;
        let _ = element_bounds;
        Some(Bounds::new(
            point(
                g.origin.x + px(g.cw * c as f32),
                g.origin.y + px(g.ch * r as f32),
            ),
            size(px(g.cw), px(g.ch)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// ---- painting ----

fn paint_grid(
    sess: &Session,
    bounds: Bounds<Pixels>,
    geo: &StdCell<Geo>,
    focused: bool,
    marked: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) {
    let font = gpui::font(brand::MONO_FONT);
    let fs = px(FONT_PX);
    let cw = {
        let ts = window.text_system();
        let id = ts.resolve_font(&font);
        ts.advance(id, fs, 'm')
            .map_or(px(FONT_PX * 0.6), |s| s.width)
    };
    let (cwf, chf) = (f32::from(cw), LINE_H);
    let origin = bounds.origin + point(px(PAD), px(PAD));
    let cols = ((f32::from(bounds.size.width) - 2. * PAD) / cwf)
        .floor()
        .max(2.) as u16;
    let rows = ((f32::from(bounds.size.height) - 2. * PAD) / chf)
        .floor()
        .max(1.) as u16;
    sess.resize(cols, rows);
    let snap = sess.snapshot();
    let sel = cx
        .try_global::<gpui_component::Theme>()
        .map_or_else(|| hsla(t::ACCENT_DEEP), |t| t.selection);
    window.paint_quad(gpui::fill(bounds, hsla(DEFAULT_BG)));
    paint_cells(&snap, origin, cwf, chf, sel, &font, window, cx);
    let at =
        |c: usize, r: usize| point(origin.x + px(cwf * c as f32), origin.y + px(chf * r as f32));
    if let Some((c, r, shape)) = snap.cursor {
        let cell = Bounds::new(at(c, r), size(cw, px(chf)));
        let accent = hsla(t::ACCENT);
        let part = match shape {
            CursorShape::Underline => {
                Bounds::new(at(c, r) + point(px(0.), px(chf - 2.)), size(cw, px(2.)))
            }
            CursorShape::Beam => Bounds::new(at(c, r), size(px(2.), px(chf))),
            _ => cell,
        };
        if focused {
            window.paint_quad(gpui::fill(
                part,
                gpui::Hsla {
                    a: if shape == CursorShape::Block { 0.6 } else { 1. },
                    ..accent
                },
            ));
        } else {
            window.paint_quad(gpui::outline(cell, accent, gpui::BorderStyle::default()));
        }
        // the preedit of the input method sits at the cursor, underlined
        if let Some(m) = marked.filter(|m| !m.is_empty()) {
            let run = TextRun {
                len: m.len(),
                font: font.clone(),
                color: hsla(t::TEXT_STRONG),
                background_color: Some(hsla(t::ELEVATED)),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(hsla(t::TEXT_STRONG)),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(m.to_string().into(), fs, &[run], Some(cw));
            let _ = line.paint(at(c, r), px(chf), TextAlign::Left, None, window, cx);
        }
        geo.set(Geo {
            origin,
            cw: cwf,
            ch: chf,
            cols: snap.cols,
            rows: snap.rows,
            cursor: (c, r),
        });
    } else {
        geo.set(Geo {
            origin,
            cw: cwf,
            ch: chf,
            cols: snap.cols,
            rows: snap.rows,
            cursor: (0, 0),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cells(
    snap: &Snap,
    origin: Point<Pixels>,
    cw: f32,
    ch: f32,
    sel: gpui::Hsla,
    font: &gpui::Font,
    window: &mut Window,
    cx: &mut App,
) {
    let fs = px(FONT_PX);
    for (r, row) in snap.cells.chunks(snap.cols).enumerate() {
        let y = origin.y + px(ch * r as f32);
        // backgrounds: one quad per run of equal colour (the selection is exactly the selected cells)
        let bg_of = |c: &crate::term::session::Cell| (c.selected, c.bg);
        let mut start = 0;
        while start < row.len() {
            let key = bg_of(&row[start]);
            let end = start + row[start..].iter().take_while(|c| bg_of(c) == key).count();
            if key.0 || key.1 != DEFAULT_BG {
                let b = Bounds::new(
                    point(origin.x + px(cw * start as f32), y),
                    size(px(cw * (end - start) as f32), px(ch)),
                );
                window.paint_quad(gpui::fill(b, if key.0 { sel } else { hsla(key.1) }));
            }
            start = end;
        }
        // text: one shaped line per row, each cell forced to the cell width
        let last = row
            .iter()
            .rposition(|c| c.ch != ' ' || c.underline || c.link || c.strike);
        let Some(last) = last else { continue };
        let mut text = String::with_capacity(last + 1);
        let mut runs: Vec<TextRun> = vec![];
        let mut key = None;
        for c in &row[..=last] {
            let ch_ = if c.ch.is_control() { ' ' } else { c.ch };
            let k = (c.fg, c.bold, c.italic, c.underline || c.link, c.strike);
            if key == Some(k) {
                if let Some(run) = runs.last_mut() {
                    run.len += ch_.len_utf8();
                }
            } else {
                let mut f = font.clone();
                if c.bold {
                    f = f.bold();
                }
                if c.italic {
                    f = f.italic();
                }
                let color = hsla(c.fg);
                runs.push(TextRun {
                    len: ch_.len_utf8(),
                    font: f,
                    color,
                    background_color: None,
                    underline: (c.underline || c.link).then_some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(color),
                        wavy: false,
                    }),
                    strikethrough: c.strike.then_some(gpui::StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(color),
                    }),
                });
                key = Some(k);
            }
            text.push(ch_);
        }
        let line = window
            .text_system()
            .shape_line(text.into(), fs, &runs, Some(px(cw)));
        let _ = line.paint(
            point(origin.x, y),
            px(ch),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

fn panel(msg: impl Into<String>) -> Div {
    div()
        .flex_1()
        .p(px(14.))
        .text_color(crate::widgets::chrome::dim())
        .child(msg.into())
}

fn bar(text: String, buttons: Vec<gpui::AnyElement>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(10.))
        .py(px(4.))
        .border_t_1()
        .border_color(hsla(t::RULE))
        .bg(hsla(t::SURFACE))
        .text_color(crate::widgets::chrome::strong())
        .child(div().flex_1().min_w(px(0.)).overflow_hidden().child(text))
        .children(buttons)
}

fn button(id: &'static str, label: &'static str) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .px(px(8.))
        .rounded(px(4.))
        .border_1()
        .border_color(hsla(t::BORDER))
        .cursor_pointer()
        .hover(|d| {
            d.bg(crate::widgets::chrome::with_alpha(
                hsla(t::TEXT_STRONG),
                0.08,
            ))
        })
        .child(label)
}

impl Render for TermPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.want_focus) {
            window.focus(&self.focus, cx);
        }
        let focused = self.focus.is_focused(window);
        let me = cx.entity();
        let mut col = div()
            .id("term-pane")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .font_family(brand::MONO_FONT)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|p, e, w, cx| p.mouse_down(e, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|p, e, w, cx| p.mouse_down(e, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|p, e, w, cx| p.mouse_down(e, w, cx)),
            )
            .on_mouse_move(cx.listener(|p, e, _, cx| p.mouse_move(e, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|p, e, _, cx| p.mouse_up(e, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|p, e, _, cx| p.mouse_up(e, cx)),
            )
            .on_scroll(cx.listener(|p, e, _, cx| p.wheel(e, cx)));
        let Some(sh) = self.shells.get(self.cur) else {
            return col.child(panel("No shell is open. Press + to start one."));
        };
        match &sh.state {
            State::Starting => col = col.child(panel("Starting the shell...")),
            State::Failed(why) => {
                col = col
                    .child(panel(format!("The shell could not start: {why}")))
                    .child(bar(
                        "Enter or the button starts it again.".into(),
                        vec![
                            button("term-retry", "retry")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(Retry)))
                                .into_any_element(),
                        ],
                    ));
            }
            State::Running(s) => {
                let (s2, geo, marked) = (s.clone(), self.geo.clone(), self.marked.clone());
                let focus = self.focus.clone();
                col = col.child(
                    div()
                        .debug_selector(|| "term-grid".to_string())
                        .flex_1()
                        .w_full()
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, (), window, cx| {
                                    paint_grid(
                                        &s2,
                                        bounds,
                                        &geo,
                                        focused,
                                        marked.as_deref(),
                                        window,
                                        cx,
                                    );
                                    window.handle_input(
                                        &focus,
                                        gpui::ElementInputHandler::new(bounds, me),
                                        cx,
                                    );
                                },
                            )
                            .size_full(),
                        ),
                );
                if let Some(end) = s.exited() {
                    col = col.child(bar(
                        format!("{end} Enter starts a new one."),
                        vec![
                            button("term-retry", "restart")
                                .on_click(cx.listener(|_, _, _, cx| cx.emit(Retry)))
                                .into_any_element(),
                        ],
                    ));
                }
            }
        }
        if let Some(u) = self.link.clone() {
            let shown: String = synthesis_harness::sanitize_for_display(&u)
                .chars()
                .take(200)
                .collect();
            let open = u.clone();
            col = col.child(bar(
                format!("Open {shown} in your browser?"),
                vec![
                    button("term-link-open", "open")
                        .on_click(cx.listener(move |p, _, _, cx| {
                            cx.open_url(&open);
                            p.link = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                    button("term-link-no", "cancel")
                        .on_click(cx.listener(|p, _, _, cx| {
                            p.link = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                ],
            ));
        }
        if let Some(n) = self.notice.clone() {
            col = col.child(bar(n, vec![]));
        }
        col
    }
}

/// The pane asks its owner (which knows the folder) for a new shell in place of an ended or failed one.
pub struct Retry;
impl EventEmitter<Retry> for TermPane {}

// ---- the TUI view's side: where shells start, the strip, the keys ----

impl TuiView {
    /// The pane, created on the first look at the Terminal tab.
    pub(super) fn ensure_term(&mut self, cx: &mut Context<Self>) -> Entity<TermPane> {
        if let Some(p) = &self.term {
            return p.clone();
        }
        let p = cx.new(TermPane::new);
        cx.subscribe(&p, |_, _, _: &Changed, cx| cx.notify())
            .detach();
        cx.subscribe(&p, |this, _, _: &Retry, cx| this.term_restart(cx))
            .detach();
        self.term = Some(p.clone());
        p
    }

    /// How a shell starts here: the session folder (or the distro's), the person's own environment.
    fn term_launch(&self) -> Result<Launch, String> {
        let lu = self
            .live
            .as_ref()
            .ok_or("Open a session to start a shell in its folder.")?;
        let e = lu
            .live
            .engine
            .as_ref()
            .ok_or("The session is still starting.")?;
        let at = match (&e.remote_distro, &e.remote_cwd) {
            (Some(d), Some(c)) => Where::Wsl {
                distro: d.clone(),
                cwd: c.clone(),
            },
            _ if e.is_remote() => {
                return Err("This engine runs somewhere a shell cannot reach.".into());
            }
            _ => Where::Here(PathBuf::from(&e.cwd)),
        };
        let windows_shell = cfg!(windows).then(|| {
            let s = synthesis_terminal::native_shell();
            (s.program.clone(), s.is_posix())
        });
        let wsl = cfg!(windows).then(|| synthesis_platform::system32("wsl.exe"));
        launch::plan(
            &at,
            &synthesis_config::Env::from_process(),
            cfg!(windows),
            wsl,
            windows_shell,
        )
    }

    /// `--shot --pty-script`: the pane shows a fixed byte script instead of a shell.
    #[cfg_attr(not(feature = "shot"), allow(dead_code))]
    pub(super) fn term_script(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let p = self.ensure_term(cx);
        p.update(cx, |p, cx| {
            p.install_script(bytes, 80, 24, cx);
            p.focus(cx);
        });
    }

    /// The first look at the tab opens one shell (once); a live session is needed for its folder.
    pub(super) fn term_tick(&mut self, cx: &mut Context<Self>) {
        let pane = self.ensure_term(cx);
        if pane.read(cx).tried || self.live.as_ref().is_none_or(|l| l.live.engine.is_none()) {
            return;
        }
        let plan = self.term_launch();
        pane.update(cx, |p, cx| p.open(plan, cx));
    }

    pub(super) fn term_new(&mut self, cx: &mut Context<Self>) {
        let pane = self.ensure_term(cx);
        let plan = self.term_launch();
        pane.update(cx, |p, cx| p.open(plan, cx));
    }

    fn term_restart(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.term.clone() else {
            return;
        };
        let plan = self.term_launch();
        pane.update(cx, |p, cx| p.restart(plan, cx));
    }

    /// The keys of the pane, before the rest of the view sees them. `Some(consumed)` ends the hook,
    /// `None` leaves the keystroke to the view's own shortcuts.
    pub(super) fn term_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let pane = self.term.clone()?;
        if !pane.read(cx).is_focused(window) {
            return None;
        }
        if self.ts.tab != super::ChatTab::Terminal {
            // the tab was left with the terminal still holding the keys: the prompt takes them back
            if let Some(lu) = &self.live {
                lu.prompt.update(cx, |s, cx| s.focus(window, cx));
            }
            return None;
        }
        if ks.modifiers.control && ks.key == "escape" {
            if let Some(lu) = &self.live {
                lu.prompt.update(cx, |s, cx| s.focus(window, cx));
            }
            return Some(true);
        }
        if ks.key == "enter" && !ks.modifiers.shift && pane.read(cx).needs_restart() {
            self.term_restart(cx);
            return Some(true);
        }
        match pane.update(cx, |p, cx| p.key(ks, cx)) {
            KeyOutcome::Taken => Some(true),
            KeyOutcome::Ime => Some(false),
            KeyOutcome::Pass => None,
        }
    }

    /// The strip over the pane and the pane or the agent's command log under it.
    pub(super) fn term_strip(&self, pane: &Entity<TermPane>, cx: &mut Context<Self>) -> Div {
        use crate::widgets::chrome::{dim, strong};
        let p = pane.read(cx);
        let chip = |id: String, label: String, on: bool| {
            div()
                .id(gpui::SharedString::from(id))
                .px(px(8.))
                .py(px(2.))
                .rounded(px(4.))
                .border_1()
                .border_color(if on {
                    hsla(t::BORDER)
                } else {
                    gpui::transparent_black()
                })
                .bg(if on {
                    hsla(t::ELEVATED)
                } else {
                    gpui::transparent_black()
                })
                .text_color(if on { strong() } else { dim() })
                .cursor_pointer()
                .child(label)
        };
        let mut strip = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .pb(px(6.))
            .flex_wrap();
        let agent_n = self
            .live
            .as_ref()
            .map_or(0, |l| l.live.view.panes.commands.len());
        strip = strip.child(
            chip(
                "term-agent".into(),
                format!("agent log ({agent_n})"),
                p.agent,
            )
            .on_click({
                let pane = pane.clone();
                move |_, _, cx| {
                    pane.update(cx, |p, cx| {
                        p.agent = true;
                        cx.notify();
                    })
                }
            }),
        );
        let tabs = p.tabs();
        let count = tabs.len();
        for (n, label, on) in tabs {
            let label: String = label.chars().take(28).collect();
            let sel = pane.clone();
            strip = strip.child(
                chip(format!("term-tab-{n}"), label, on)
                    .on_click(move |_, _, cx| sel.update(cx, |p, cx| p.select(n, cx))),
            );
            if on {
                let close = pane.clone();
                strip = strip.child(
                    chip(format!("term-close-{n}"), "x".into(), false)
                        .on_click(move |_, _, cx| close.update(cx, |p, cx| p.close(n, cx))),
                );
            }
        }
        if count < MAX_SHELLS {
            strip = strip.child(
                chip("term-new".into(), "+".into(), false)
                    .on_click(cx.listener(|this, _, _, cx| this.term_new(cx))),
            );
        }
        strip
    }
}
