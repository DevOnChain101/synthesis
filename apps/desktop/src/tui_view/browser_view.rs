//! The Browser tab as a page view (spec `docs/specs/terminal-browser.md` 2.3, 2.6, PR B2, VIEW ONLY): the live
//! frames of the session's browser (`BrowserRuntime::view` / `frames`) drawn 1:1 in device pixels into the pane (the page's viewport follows the pane, so it needs no letterbox), a URL bar
//! whose navigation passes the browser's own `UrlPolicy`, back / forward / reload, the browser's tabs and the
//! agent cursor. The person's clicks, wheel, keys and paste reach the page (B3) only while the page holds the pane's
//! focus and only as `PageView::input`; the agent never goes through this path (see the spec, B3).
//!
//! Frames are the person's own view: they bypass `guard_capture`, and they never go anywhere but the pane's one
//! decoded slot (never an event, the store, a log or the model). Decoding runs off the UI thread, the frame
//! size is capped before the decode and only the newest frame is kept.
//!
//! The pane shows the browser once one runs (`BrowserTap`: the agent launched it, or the person pressed "Start browser",
//! which asks the engine's launcher; the pane never launches anything itself). Until then the tab says there is no
//! browser, offers Start (or says why it cannot) and keeps its earlier body (the saved screenshots and the call list).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{Receiver, Sender, channel};
use futures::future::BoxFuture;
use gpui::{
    AnyElement, AvailableSpace, Bounds, Context, Corners, Entity, FocusHandle, Focusable as _,
    Keystroke, MouseButton, RenderImage, Window, canvas, div, point, prelude::*, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use synthesis_browser::{
    ALT, BrowserError, CTRL, Frame, META, PageButton, PageInput, PageView, SHIFT, UrlPolicy,
    ViewMetrics,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_harness::sanitize_for_display;
use synthesis_tools::BrowserTap;
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};
use tokio::task::AbortHandle;

use super::{ChatTab, TuiView};
use crate::theme::hsla;
use crate::widgets::chrome::{dim, icon, strong, with_alpha};
use crate::widgets::panes;

/// Largest side of a frame the pane decodes (spec 2.3); a bigger one is refused before the decode.
pub const MAX_SIDE: u32 = 4096;
/// The decoder may not allocate more than this for one frame (4096 x 4096 RGBA is 64 MiB).
const MAX_ALLOC: u64 = 80 << 20;
/// The cast size before the pane has reported its size (then the page keeps the browser's configured viewport).
const VIEW_W: u32 = 1600;
const VIEW_H: u32 = 1000;
/// How often the pane asks the browser for its URL, title and tabs.
const POLL: Duration = Duration::from_secs(1);
/// A page controls its URL and title: at most this many characters of each reach the bar and the tab strip.
pub const MAX_TEXT: usize = 2048;
/// An address typed into the bar longer than this is refused at Enter.
pub const MAX_TYPED: usize = 2048;
/// Tabs listed in the strip (a page can open popups without end).
const MAX_TABS: usize = 32;
/// An address offered to the person's own browser is shown whole next to the button, so it must be short.
const MAX_OPEN: usize = 200;
/// A pane size or scale change must hold still this long before the page is resized (a window drag is one resize).
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(if cfg!(test) { 400 } else { 250 });
/// A failed `view(true)` is tried again after this long, this many times in all.
const START_RETRY: Duration = Duration::from_millis(if cfg!(test) { 400 } else { 5000 });
const MAX_STARTS: u32 = 5;
/// Polls without any frame (the view is on) after which the pane asks the browser to start casting again.
const REASSERT_POLLS: u32 = 5;

/// The first `n` characters (never cuts a character in two).
pub fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// What the browser tells the pane about its pages is the page's own text: bounded before it is stored or drawn.
fn capped(mut p: PageState) -> PageState {
    p.url = clip(&p.url, MAX_TEXT);
    p.title = clip(&p.title, MAX_TEXT);
    // at most MAX_TABS tabs, in a window that always holds the active one (it must stay visible and highlighted)
    if p.tabs.len() > MAX_TABS {
        let start = p
            .active
            .saturating_sub(MAX_TABS - 1)
            .min(p.tabs.len() - MAX_TABS);
        p.tabs = p.tabs.split_off(start);
        p.tabs.truncate(MAX_TABS);
        p.ids = p.ids.split_off(start.min(p.ids.len()));
        p.ids.truncate(MAX_TABS);
        p.offset = start;
    }
    p.tabs.iter_mut().for_each(|t| *t = clip(t, MAX_TEXT));
    p
}

// ---- pure parts -------------------------------------------------------------------------------------------

/// Where a frame of `fw` x `fh` device pixels goes in a pane whose top-left is `(ox, oy)` (logical pixels) at `scale`:
/// 1:1 (never resampled by the window), the origin on a whole device pixel, so the text stays as sharp as Chrome
/// drew it. Where the frame does not reach (a resize in flight) the pane's own background shows. `None` for a size or
/// scale that is not a positive finite number.
pub fn place(ox: f32, oy: f32, fw: f32, fh: f32, scale: f32) -> Option<(f32, f32, f32, f32)> {
    let ok = |v: f32| v.is_finite() && v > 0.0;
    if !(ok(fw) && ok(fh) && ok(scale) && ox.is_finite() && oy.is_finite()) {
        return None;
    }
    let snap = |v: f32| (v * scale).round() / scale;
    Some((snap(ox), snap(oy), fw / scale, fh / scale))
}

/// Where the pane's page area is in the window (logical pixels): left, top, width, height.
type Stage = (f32, f32, f32, f32);

/// A window position as CSS pixels of the page. The viewport is the pane at 1:1 (`ViewMetrics`), so this is the offset
/// from the pane's corner. Outside the page: `None`, or with `clamp` (a button released after a drag out) the nearest edge.
pub fn to_page(pos: (f32, f32), (ox, oy, w, h): Stage, clamp: bool) -> Option<(f64, f64)> {
    let (vw, vh) = (w.floor(), h.floor());
    let (x, y) = (pos.0 - ox, pos.1 - oy);
    if ![x, y, vw, vh].iter().all(|v| v.is_finite()) || vw < 1. || vh < 1. {
        return None;
    }
    let inside = (0. ..vw).contains(&x) && (0. ..vh).contains(&y);
    if !inside && !clamp {
        return None;
    }
    Some((
        f64::from(x.clamp(0., vw - 1.)),
        f64::from(y.clamp(0., vh - 1.)),
    ))
}

/// What a keystroke means while the page holds the pane's focus.
#[derive(Debug, PartialEq)]
pub enum PageKey {
    /// Esc: the page gives the keys back.
    Release,
    /// The paste chord: the pane reads the clipboard (only now) and sends it as text.
    Paste,
    Send(PageInput),
    /// The mod key (Cmd on macOS, else Ctrl) with T / W, and Ctrl+Tab (`true`: with Shift, backwards): the tabs. Only
    /// while the page holds the keys, so Ctrl+T elsewhere stays the target cycle.
    NewTab,
    CloseTab,
    CycleTab(bool),
    /// Not the page's (an app chord): the rest of the window sees it.
    Pass,
    /// A key the page has no mapping for: swallowed, so it cannot reach the chat input by accident.
    Ignore,
}

/// `mac`: Cmd is the shortcut key (else Ctrl). Every shortcut but select all, undo, redo and paste stays the app's.
pub fn page_key(ks: &Keystroke, mac: bool) -> PageKey {
    let m = &ks.modifiers;
    let mods = [
        (m.alt, ALT),
        (m.control, CTRL),
        (m.platform, META),
        (m.shift, SHIFT),
    ]
    .into_iter()
    .fold(0, |a, (on, bit)| if on { a | bit } else { a });
    let key = ks.key.as_str();
    if key == "escape" && !m.alt && !m.platform {
        return PageKey::Release;
    }
    if key == "tab" && m.control && !m.alt && !m.platform {
        return PageKey::CycleTab(m.shift);
    }
    // Ctrl+C on macOS stays the app's: it interrupts the agent
    if mac && m.control && key == "c" {
        return PageKey::Pass;
    }
    if if mac { m.platform } else { m.control } {
        return match key {
            "v" => PageKey::Paste,
            "t" if !m.alt && !m.shift => PageKey::NewTab,
            "w" if !m.alt && !m.shift => PageKey::CloseTab,
            "a" | "z" => PageKey::Send(PageInput::Key {
                key: key.into(),
                mods,
            }),
            _ => PageKey::Pass,
        };
    }
    let named = match key {
        "enter" => "Enter",
        "tab" => "Tab",
        "escape" => "Escape",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "space" => " ",
        _ => "",
    };
    let one = |s: &str| (s.chars().count() == 1).then(|| s.chars().next()).flatten();
    let sent = if !named.is_empty() {
        Some(named.to_owned())
    } else if m.control || m.platform {
        // a chord: the key itself, no text
        one(key).map(String::from)
    } else {
        // the character that was typed (shift, Option and layouts included); outside ASCII it goes in as text
        match ks.key_char.as_deref().and_then(one) {
            Some(c) if c.is_ascii() && !c.is_ascii_control() => Some(c.to_string()),
            Some(c) if !c.is_control() => return PageKey::Send(PageInput::Text(c.to_string())),
            _ => None,
        }
    };
    sent.map_or(PageKey::Ignore, |key| {
        PageKey::Send(PageInput::Key { key, mods })
    })
}

/// Input waiting for the browser: moves and wheel turns merge into the one behind them (latest wins), so a flood of
/// pointer events is one call in flight and at most one waiting; the rest is bounded.
#[derive(Default)]
struct InputQueue {
    q: VecDeque<PageInput>,
    running: bool,
}

const MAX_PENDING: usize = 64;

impl InputQueue {
    /// Queues `ev`; true when the caller must start the sender (none runs).
    fn push(&mut self, ev: PageInput) -> bool {
        match (self.q.back_mut(), &ev) {
            (Some(last @ PageInput::Move { .. }), PageInput::Move { .. }) => *last = ev,
            (
                Some(PageInput::Wheel { dx, dy, mods, .. }),
                PageInput::Wheel {
                    dx: ax,
                    dy: ay,
                    mods: m2,
                    ..
                },
            ) if mods == m2 => {
                (*dx, *dy) = (*dx + ax, *dy + ay);
            }
            _ => {
                if self.q.len() >= MAX_PENDING {
                    self.q
                        .retain(|e| !matches!(e, PageInput::Move { .. } | PageInput::Wheel { .. }));
                }
                if self.q.len() < MAX_PENDING {
                    self.q.push_back(ev);
                }
            }
        }
        !std::mem::replace(&mut self.running, true)
    }

    fn pop(&mut self) -> Option<PageInput> {
        let e = self.q.pop_front();
        self.running = e.is_some();
        e
    }
}

/// What the person typed in the URL bar as an address: no scheme means https. `localhost:3000` is a host and a
/// port, not the scheme `localhost`.
pub fn bar_address(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("type an address".into());
    }
    if s.chars().any(char::is_control) {
        return Err("the address has control characters".into());
    }
    let scheme = s.split_once(':').and_then(|(p, rest)| {
        let word = !p.is_empty()
            && p.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        let known = [
            "about",
            "data",
            "javascript",
            "file",
            "blob",
            "mailto",
            "view-source",
            "chrome",
        ];
        (word && (rest.starts_with("//") || known.contains(&p.to_ascii_lowercase().as_str())))
            .then_some(())
    });
    Ok(if scheme.is_some() {
        s.to_owned()
    } else {
        format!("https://{s}")
    })
}

/// The address the URL bar may navigate to, or why not: the browser's own navigation policy (http and https
/// only, no private, link-local or metadata address unless the user allowed it, `deny_domains`...), except that the
/// person may open loopback (localhost, 127.0.0.1, ::1).
pub fn check_bar(policy: &UrlPolicy, raw: &str) -> Result<String, String> {
    let url = bar_address(raw)?;
    // typed by the person: loopback is allowed here (the agent's own navigation stays strict)
    policy.check_navigation_person(&url)?;
    Ok(url)
}

/// A typed address that was refused: what to say, and an http(s) address the person may hand to their own browser.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub text: String,
    /// Offered only for an address the PERSON typed and that is http or https; never for a URL the agent produced.
    pub open: Option<String>,
}

pub fn refusal(why: String, typed: &str) -> Refusal {
    let open = bar_address(typed).ok().filter(|u| {
        let l = u.to_ascii_lowercase();
        // plain ASCII that display sanitising leaves alone: no bidi override, zero-width or other invisible character
        // (and no non-ASCII host), so what is shown is exactly what is opened
        (l.starts_with("http://") || l.starts_with("https://"))
            && u.is_ascii()
            && sanitize_for_display(u) == *u
            && !l.contains(char::is_whitespace)
            && l.chars().count() <= MAX_OPEN
    });
    Refusal {
        text: sanitize_for_display(&why),
        open,
    }
}

/// A frame ready to draw. The pixels are only here, in one slot.
#[derive(Clone)]
pub struct Decoded {
    pub image: Arc<RenderImage>,
    pub w: u32,
    pub h: u32,
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decoded({}x{})", self.w, self.h)
    }
}

/// JPEG to RGBA, refusing a frame wider or taller than [`MAX_SIDE`] before any pixel is decoded.
pub fn decode(jpeg: &[u8]) -> Result<image::RgbaImage, String> {
    use image::{ImageFormat, ImageReader};
    let reader = || ImageReader::with_format(std::io::Cursor::new(jpeg), ImageFormat::Jpeg);
    let (w, h) = reader()
        .into_dimensions()
        .map_err(|e| format!("frame not readable: {e}"))?;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!(
            "frame of {w}x{h} refused (limit {MAX_SIDE} per side)"
        ));
    }
    let mut r = reader();
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_ALLOC);
    r.limits(limits);
    r.decode()
        .map(|i| i.to_rgba8())
        .map_err(|e| format!("frame not readable: {e}"))
}

/// The decoded frame as gpui wants it (BGRA).
pub fn to_decoded(mut rgba: image::RgbaImage) -> Decoded {
    let (w, h) = rgba.dimensions();
    for px in rgba.pixels_mut() {
        px.0.swap(0, 2);
    }
    Decoded {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])),
        w,
        h,
    }
}

// ---- what the pane reads from the browser -----------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageState {
    pub url: String,
    pub title: String,
    /// Titles (or URLs) of the browser's tabs, oldest first.
    pub tabs: Vec<String>,
    /// The browser's id of each tab in `tabs`: sent back with a switch or close, so an index that points at another
    /// tab by then (the agent or a popup changed the list) is refused instead of acted on.
    pub ids: Vec<String>,
    /// Index (in the browser's tab order) of the active tab.
    pub active: usize,
    /// Index of `tabs[0]` in the browser's tab order: only [`MAX_TABS`] are kept, a window that holds the active tab.
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Go(String),
    Back,
    Forward,
    Reload,
    /// Switch to the tab at this index of the browser's order, which must still have this id.
    Tab(usize, String),
    /// A blank tab, made current.
    NewTab,
    Close(usize, String),
}

/// The tab Ctrl+Tab (`back`: Ctrl+Shift+Tab) goes to, in the browser's own index; `None` with fewer than two tabs.
pub fn next_tab(p: &PageState, back: bool) -> Option<Act> {
    let n = p.tabs.len();
    let at = p.active.checked_sub(p.offset).filter(|a| *a < n)?;
    let to = if back { (at + n - 1) % n } else { (at + 1) % n };
    (n > 1).then(|| Act::Tab(p.offset + to, p.ids.get(to).cloned().unwrap_or_default()))
}

pub trait Feed: Send {
    /// The newest frame (the browser keeps one slot), `None` when the browser is gone.
    fn next(&mut self) -> BoxFuture<'_, Option<Frame>>;
}

/// The browser the pane looks at (a fake in the tests and the shots).
pub trait Source: Send + Sync + 'static {
    fn ready(&self) -> bool;
    fn check(&self, raw: &str) -> Result<String, String>;
    /// Starts (`on`) or stops the live view; `fit` is the page viewport the pane wants (`None`: not known yet).
    fn view(&self, on: bool, fit: Option<ViewMetrics>) -> BoxFuture<'static, Result<(), String>>;
    fn frames(&self) -> Option<Box<dyn Feed>>;
    fn page(&self) -> BoxFuture<'static, Result<PageState, String>>;
    fn act(&self, a: Act) -> BoxFuture<'static, Result<(), String>>;
    /// The person's input to the page.
    fn input(&self, ev: PageInput) -> BoxFuture<'static, Result<(), String>>;
}

/// The real thing: the browser the agent launched, found through the tap (weakly: the pane never keeps Chrome alive) and
/// reached only through its view-only handle.
pub struct RuntimeSource(pub Arc<BrowserTap>);

impl RuntimeSource {
    fn pv(&self) -> Option<PageView> {
        self.0.view()
    }
}

fn button_of(b: MouseButton) -> PageButton {
    match b {
        MouseButton::Right => PageButton::Right,
        MouseButton::Middle => PageButton::Middle,
        _ => PageButton::Left,
    }
}

fn why(e: BrowserError) -> String {
    sanitize_for_display(&e.to_string())
}

struct Stream(synthesis_browser::FrameStream);

impl Feed for Stream {
    fn next(&mut self) -> BoxFuture<'_, Option<Frame>> {
        Box::pin(self.0.next())
    }
}

impl Source for RuntimeSource {
    fn ready(&self) -> bool {
        self.pv().is_some()
    }

    fn check(&self, raw: &str) -> Result<String, String> {
        let rt = self.pv().ok_or("no browser is running")?;
        check_bar(rt.policy(), raw)
    }

    fn view(&self, on: bool, fit: Option<ViewMetrics>) -> BoxFuture<'static, Result<(), String>> {
        let rt = self.pv();
        Box::pin(async move {
            match (rt, fit) {
                (Some(rt), Some(m)) if on => rt.view_fit(true, m).await.map_err(why),
                (Some(rt), _) => rt.view(on, VIEW_W, VIEW_H).await.map_err(why),
                (None, _) => Ok(()),
            }
        })
    }

    fn frames(&self) -> Option<Box<dyn Feed>> {
        Some(Box::new(Stream(self.pv()?.frames())))
    }

    fn page(&self) -> BoxFuture<'static, Result<PageState, String>> {
        let rt = self.pv();
        Box::pin(async move {
            let tabs = rt
                .ok_or("the browser is closed")?
                .tabs()
                .await
                .map_err(why)?;
            let at = tabs.iter().position(|t| t.active).unwrap_or(0);
            let show = |s: &synthesis_browser::UntrustedText| sanitize_for_display(s.expose());
            Ok(PageState {
                url: tabs.get(at).map(|t| show(&t.page.url)).unwrap_or_default(),
                title: tabs
                    .get(at)
                    .map(|t| show(&t.page.title))
                    .unwrap_or_default(),
                ids: tabs.iter().map(|t| t.id.clone()).collect(),
                tabs: tabs
                    .iter()
                    .map(|t| {
                        let title = show(&t.page.title);
                        if title.is_empty() {
                            show(&t.page.url)
                        } else {
                            title
                        }
                    })
                    .collect(),
                active: at,
                offset: 0,
            })
        })
    }

    fn act(&self, a: Act) -> BoxFuture<'static, Result<(), String>> {
        let rt = self.pv();
        Box::pin(async move {
            let rt = rt.ok_or("the browser is closed")?;
            match a {
                // navigate() checks the policy again before it sends anything
                Act::Go(u) => rt
                    .navigate(&u, synthesis_browser::WaitUntil::DomContentLoaded)
                    .await
                    .map(drop),
                Act::Back => rt.back().await.map(drop),
                Act::Forward => rt.forward().await.map(drop),
                Act::Reload => rt.reload().await.map(drop),
                Act::Tab(i, id) => rt.select_tab(i, &id).await,
                Act::NewTab => rt.new_tab(None).await.map(drop),
                Act::Close(i, id) => rt.close_tab(i, &id).await,
            }
            .map_err(why)
        })
    }

    fn input(&self, ev: PageInput) -> BoxFuture<'static, Result<(), String>> {
        let rt = self.pv();
        Box::pin(async move {
            rt.ok_or("the browser is closed")?
                .input(ev)
                .await
                .map_err(why)
        })
    }
}

// ---- the pane's state -------------------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    frame: Option<Decoded>,
    frame_err: Option<String>,
    page: Option<PageState>,
    /// The last action (or the view itself) failed: shown under the bar.
    note: Option<String>,
    /// A new tab was made (not refused): the address bar takes the focus.
    focus_bar: bool,
}

struct Link {
    src: Arc<dyn Source>,
    handle: Handle,
    /// Which tap it came from (0: a fixed source of a test or a shot, never replaced).
    tap: usize,
    /// The state the pane wants (`true`: shown). One worker task applies the latest wish, so view calls never overlap.
    on: bool,
    want: watch::Sender<bool>,
    /// Asks the worker to point the view at the current tab again (it never follows a tab by itself).
    retarget: Arc<Notify>,
}

/// The person's "Start browser".
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Launch {
    #[default]
    Idle,
    Starting,
    Failed(String),
}

pub struct BrowserPane {
    link: Option<Link>,
    launch: Arc<Mutex<Launch>>,
    shared: Arc<Mutex<Shared>>,
    wake: Sender<()>,
    /// Taken once by the window, which repaints on every `()`.
    pub wake_rx: Option<Receiver<()>>,
    pub bar: Option<Entity<InputState>>,
    bar_url: String,
    pub refusal: Option<Refusal>,
    /// The image last handed to the window, to free it from the atlas when the next one comes.
    painted: Option<Arc<RenderImage>>,
    /// The page viewport the pane wants (its size at the window's scale), reported by the drawing code.
    fit: Arc<watch::Sender<Option<ViewMetrics>>>,
    /// Where the page area is, reported by the drawing code (to map the pointer).
    stage: Arc<Mutex<Option<Stage>>>,
    inputs: Arc<Mutex<InputQueue>>,
    /// The page area's focus: while it holds it, keys go to the page.
    pub focus: Option<FocusHandle>,
    /// `focus` is held (read each frame): the accent border and the footer follow it.
    pub captured: bool,
}

impl Default for BrowserPane {
    fn default() -> Self {
        let (wake, rx) = channel(1);
        Self {
            link: None,
            launch: Arc::default(),
            shared: Arc::default(),
            wake,
            wake_rx: Some(rx),
            bar: None,
            bar_url: String::new(),
            refusal: None,
            painted: None,
            fit: Arc::new(watch::channel(None).0),
            stage: Arc::default(),
            inputs: Arc::default(),
            focus: None,
            captured: false,
        }
    }
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        self.sync(false);
    }
}

fn lock_q(m: &Mutex<InputQueue>) -> MutexGuard<'_, InputQueue> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl BrowserPane {
    pub fn is_attached(&self) -> bool {
        self.link.is_some()
    }

    /// A browser the pane shows from now on (the tests and shots hand a fake one; the window uses [`Self::ensure`]).
    pub fn attach(&mut self, src: Arc<dyn Source>, handle: Handle) {
        self.detach();
        let (want, rx) = watch::channel(false);
        let retarget = Arc::new(Notify::new());
        handle.spawn(worker(
            src.clone(),
            self.shared.clone(),
            self.wake.clone(),
            rx,
            self.fit.subscribe(),
            retarget.clone(),
        ));
        self.link = Some(Link {
            src,
            handle,
            tap: 0,
            on: false,
            want,
            retarget,
        });
    }

    pub fn launch_state(&self) -> Launch {
        self.launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// "Start browser": runs `start` (the engine's launch) off the UI thread. Success is picked up by [`Self::ensure`]
    /// once the tap holds the browser; a failure is kept to be shown in the pane.
    pub fn launch_with(&mut self, handle: &Handle, start: BoxFuture<'static, Result<(), String>>) {
        let (launch, mut wake) = (self.launch.clone(), self.wake.clone());
        {
            let mut l = launch.lock().unwrap_or_else(|e| e.into_inner());
            if *l == Launch::Starting {
                return;
            }
            *l = Launch::Starting;
        }
        handle.spawn(async move {
            let r = start.await;
            *launch.lock().unwrap_or_else(|e| e.into_inner()) = match r {
                Ok(()) => Launch::Idle,
                Err(e) => Launch::Failed(sanitize_for_display(&e)),
            };
            let _ = wake.try_send(());
        });
    }

    /// Follows the session's browser: attached once the agent has launched it, dropped when it is gone.
    pub fn ensure(&mut self, tap: &Arc<BrowserTap>, handle: &Handle) {
        let id = Arc::as_ptr(tap) as usize;
        let ready = tap.view().is_some();
        match &self.link {
            Some(l) if l.tap == 0 => {}
            Some(l) if ready && l.tap == id => {}
            Some(_) => {
                self.detach();
                if ready {
                    self.attach(Arc::new(RuntimeSource(tap.clone())), handle.clone());
                    if let Some(l) = &mut self.link {
                        l.tap = id;
                    }
                }
            }
            None if ready => {
                self.attach(Arc::new(RuntimeSource(tap.clone())), handle.clone());
                if let Some(l) = &mut self.link {
                    l.tap = id;
                }
            }
            None => {}
        }
    }

    fn detach(&mut self) {
        self.sync(false);
        self.link = None;
        *lock(&self.shared) = Shared::default();
        self.refusal = None;
    }

    /// The pane is shown (`want`) or not. The worker starts the view of the browser (`view(true)`, the frame pump and
    /// the page poll) or stops it (`view(false)`, nothing decoded or held any more). The calls are applied one at a
    /// time and the last wish wins, so a quick hide and show cannot leave the browser casting the wrong way.
    pub fn sync(&mut self, want: bool) {
        let Some(l) = &mut self.link else { return };
        if want == l.on {
            return;
        }
        l.on = want;
        if !want {
            lock(&self.shared).frame = None; // a hidden pane holds no pixels
        }
        l.want.send_replace(want);
    }

    /// What to draw now.
    pub fn snapshot(
        &self,
    ) -> (
        Option<Decoded>,
        Option<String>,
        Option<PageState>,
        Option<String>,
    ) {
        let s = lock(&self.shared);
        (
            s.frame.clone(),
            s.frame_err.clone(),
            s.page.clone(),
            s.note.clone(),
        )
    }

    /// The frame on the screen changed: the old image leaves the window's atlas.
    pub fn take_stale(&mut self, shown: Option<&Decoded>) -> Option<Arc<RenderImage>> {
        let now = shown.map(|d| d.image.clone());
        let same = match (&self.painted, &now) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return None;
        }
        std::mem::replace(&mut self.painted, now)
    }

    /// Enter in the URL bar: the address is checked against the browser's navigation policy and only then sent.
    /// An unedited bar is a no-op: it shows the page's address redacted (`redact_url`), which is not always the real
    /// one; the Reload button reloads the real page.
    pub fn go(&mut self, raw: &str) {
        let Some(l) = &self.link else { return };
        if raw == self.bar_url {
            self.refusal = None;
            return;
        }
        if raw.chars().count() > MAX_TYPED {
            self.refusal = Some(Refusal {
                text: format!("the address is longer than {MAX_TYPED} characters"),
                open: None,
            });
            return;
        }
        match l.src.check(raw) {
            Err(e) => self.refusal = Some(refusal(e, raw)),
            Ok(url) => {
                self.refusal = None;
                self.act(Act::Go(url));
            }
        }
    }

    /// The person's input for the page (from this pane's own handlers, never the agent).
    /// Nothing is sent without a browser; a flood is merged (`InputQueue`); a refusal (the agent is using the tab, a
    /// password field) shows under the bar and drops what was waiting.
    pub fn input(&mut self, ev: PageInput) {
        let Some(l) = &self.link else { return };
        if !lock_q(&self.inputs).push(ev) {
            return;
        }
        let (src, shared, q, mut wake) = (
            l.src.clone(),
            self.shared.clone(),
            self.inputs.clone(),
            self.wake.clone(),
        );
        l.handle.spawn(async move {
            loop {
                let Some(ev) = lock_q(&q).pop() else { break };
                let r = src.input(ev).await;
                let busy = BrowserError::Busy.to_string();
                let mut sh = lock(&shared);
                match r {
                    Ok(()) if sh.note.as_deref() == Some(&busy) => sh.note = None,
                    Ok(()) => continue,
                    Err(e) => {
                        sh.note = Some(e);
                        lock_q(&q).q.clear();
                    }
                }
                drop(sh);
                let _ = wake.try_send(());
            }
        });
    }

    /// True once after a new tab was really opened (a refusal, busy or the cap, leaves the focus alone).
    pub fn take_focus_bar(&self) -> bool {
        std::mem::take(&mut lock(&self.shared).focus_bar)
    }

    /// The pane shows a note under the bar (a refusal of input or of an action).
    pub fn has_note(&self) -> bool {
        lock(&self.shared).note.is_some()
    }

    /// The person's paste: text over the browser's limit is not sent, and the pane says so.
    pub fn paste(&mut self, t: String) {
        if t.chars().count() > synthesis_browser::MAX_TEXT {
            lock(&self.shared).note = Some(format!(
                "the clipboard text is longer than {} characters: not pasted",
                synthesis_browser::MAX_TEXT
            ));
            let _ = self.wake.try_send(());
        } else {
            self.input(PageInput::Text(t));
        }
    }

    /// The pointer at `pos` (window pixels) as page input; `None` when it is not over the page.
    pub fn pointer(&self, pos: (f32, f32), clamp: bool) -> Option<(f64, f64)> {
        to_page(pos, (*self.stage.lock().ok()?)?, clamp)
    }

    /// The bar text changed: a refusal (and its "Open in your browser" button) is about the old text.
    pub fn bar_edited(&mut self) {
        self.refusal = None;
    }

    /// Closes the active tab (the browser keeps the last one).
    pub fn close_active(&mut self) {
        if let Some(p) = self.snapshot().2 {
            let id = p.ids.get(p.active.saturating_sub(p.offset)).cloned();
            self.act(Act::Close(p.active, id.unwrap_or_default()));
        }
    }

    /// Ctrl+Tab: the next (or previous) tab.
    pub fn cycle(&mut self, back: bool) {
        if let Some(a) = self.snapshot().2.and_then(|p| next_tab(&p, back)) {
            self.act(a);
        }
    }

    pub fn act(&mut self, a: Act) {
        let Some(l) = &self.link else { return };
        let (src, shared, mut wake) = (l.src.clone(), self.shared.clone(), self.wake.clone());
        let again = l.retarget.clone();
        l.handle.spawn(async move {
            let opened = a == Act::NewTab;
            let switched = matches!(a, Act::Tab(..) | Act::NewTab | Act::Close(..));
            let r = src.act(a).await;
            // the view never follows a tab by itself: after a switch the worker points it at the new one
            if r.is_ok() && switched {
                again.notify_one();
            }
            let ok_new = r.is_ok() && opened;
            {
                let mut sh = lock(&shared);
                sh.focus_bar |= ok_new;
                sh.note = r.err();
            }
            if let Ok(p) = src.page().await {
                lock(&shared).page = Some(capped(p));
            }
            let _ = wake.try_send(());
        });
    }
}

/// Applies the pane's latest wish to the browser, one call at a time (a new wish waits for the call in flight, then
/// the last one wins). While the view is on, it runs the frame pump and the page poll.
async fn worker(
    src: Arc<dyn Source>,
    shared: Arc<Mutex<Shared>>,
    mut wake: Sender<()>,
    mut want: watch::Receiver<bool>,
    mut fit: watch::Receiver<Option<ViewMetrics>>,
    retarget: Arc<Notify>,
) {
    let mut applied = false;
    // a start that failed is tried again every START_RETRY, MAX_STARTS times in all
    let (mut tries, mut retry) = (0u32, false);
    let mut tasks: Vec<AbortHandle> = vec![];
    let stop = |tasks: &mut Vec<AbortHandle>| tasks.drain(..).for_each(|t| t.abort());
    loop {
        let on = *want.borrow_and_update();
        if on != applied {
            applied = on;
            retry = false;
            stop(&mut tasks);
            if on {
                let m = *fit.borrow_and_update();
                match src.view(true, m).await {
                    Ok(()) => {
                        tries = 0;
                        lock(&shared).note = None;
                        if let Some(feed) = src.frames() {
                            tasks.push(
                                tokio::spawn(pump(feed, shared.clone(), wake.clone()))
                                    .abort_handle(),
                            );
                        }
                        tasks.push(
                            tokio::spawn(poll(
                                src.clone(),
                                shared.clone(),
                                wake.clone(),
                                retarget.clone(),
                            ))
                            .abort_handle(),
                        );
                    }
                    Err(e) => {
                        tries += 1;
                        retry = tries < MAX_STARTS;
                        let more = if retry {
                            "trying again"
                        } else {
                            "giving up: hide and show the tab to retry"
                        };
                        lock(&shared).note = Some(format!("{e} ({more})"));
                    }
                }
                let _ = wake.try_send(());
            } else {
                lock(&shared).frame = None;
                let _ = src.view(false, None).await;
            }
            continue; // the wish may have changed during the call
        }
        tokio::select! {
            r = want.changed() => {
                if r.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep(START_RETRY), if retry => {
                // the start failed: ask again (the wish is still `on`)
                retry = false;
                applied = false;
            }
            _ = retarget.notified(), if applied => {
                let m = *fit.borrow_and_update();
                if let Err(e) = src.view(true, m).await {
                    lock(&shared).note = Some(e);
                }
            }
            r = fit.changed(), if applied => {
                if r.is_err() {
                    break;
                }
                // wait until the size holds still, then resize the page once
                while let Ok(r) = tokio::time::timeout(RESIZE_DEBOUNCE, fit.changed()).await {
                    if r.is_err() {
                        break;
                    }
                }
                // hidden during the wait: the hide is next, a resize now would switch the view back on
                if want.has_changed().unwrap_or(true) {
                    continue;
                }
                let m = *fit.borrow_and_update();
                if let Err(e) = src.view(true, m).await {
                    lock(&shared).note = Some(e);
                }
            }
        }
    }
    // the pane is gone: no frame pump, and the browser stops casting
    stop(&mut tasks);
    if applied {
        let _ = src.view(false, None).await;
    }
}

/// Decodes the newest frame, over and over; only the latest is kept.
async fn pump(mut feed: Box<dyn Feed>, shared: Arc<Mutex<Shared>>, mut wake: Sender<()>) {
    while let Some(f) = feed.next().await {
        // off the async threads: a decode of a large frame must not stall the browser's own calls
        let r = tokio::task::spawn_blocking(move || decode(&f.jpeg).map(to_decoded)).await;
        let mut s = lock(&shared);
        match r {
            Ok(Ok(d)) => (s.frame, s.frame_err) = (Some(d), None),
            Ok(Err(e)) => s.frame_err = Some(e),
            Err(_) => s.frame_err = Some("frame not readable".into()),
        }
        drop(s);
        let _ = wake.try_send(());
    }
}

/// Every second: URL, title and tabs. A changed active tab or number of tabs (the agent switched, a popup opened,
/// a tab closed) asks the worker to point the view at the current tab again; so does a view that has shown no frame
/// for [`REASSERT_POLLS`] polls.
async fn poll(
    src: Arc<dyn Source>,
    shared: Arc<Mutex<Shared>>,
    mut wake: Sender<()>,
    retarget: Arc<Notify>,
) {
    let mut last: Option<(usize, usize)> = None;
    let mut blank = 0;
    loop {
        if let Ok(p) = src.page().await {
            // the signature is the browser's own, before the strip is bounded
            let sig = (p.active, p.tabs.len());
            let p = capped(p);
            if last.is_some_and(|l| l != sig) {
                retarget.notify_one();
            }
            last = Some(sig);
            let mut s = lock(&shared);
            blank = if s.frame.is_some() { 0 } else { blank + 1 };
            s.page = Some(p);
            drop(s);
            if blank >= REASSERT_POLLS {
                blank = 0;
                retarget.notify_one();
            }
            let _ = wake.try_send(());
        }
        tokio::time::sleep(POLL).await;
    }
}

// ---- the window's side ------------------------------------------------------------------------------------

impl TuiView {
    /// Per frame: follows the agent's browser, starts or stops the view with the tab, keeps the URL bar.
    pub(super) fn browser_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want =
            self.shown && self.active && self.pane_tab() == ChatTab::Browser && !self.previewing();
        if let Some((tap, h)) = self
            .live
            .as_ref()
            .and_then(|l| l.live.engine.as_ref())
            .and_then(|e| e.browser_access())
        {
            self.ts.bv.ensure(&tap, &h);
        }
        let bp = &mut self.ts.bv;
        if !bp.is_attached() {
            return;
        }
        bp.sync(want);
        if let Some(mut rx) = bp.wake_rx.take() {
            cx.spawn(async move |this, cx| {
                while rx.next().await.is_some() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        if !want {
            return;
        }
        let (frame, _, page, _) = bp.snapshot();
        if let Some(old) = bp.take_stale(frame.as_ref()) {
            let _ = window.drop_image(old);
        }
        if bp.bar.is_none() {
            let bar =
                cx.new(|cx| InputState::new(window, cx).placeholder("Address (http or https)"));
            cx.subscribe_in(&bar, window, |this, bar, ev: &InputEvent, _, cx| match ev {
                InputEvent::PressEnter { .. } => {
                    let raw = bar.read(cx).value().to_string();
                    this.ts.bv.go(&raw);
                    cx.notify();
                }
                InputEvent::Change => {
                    this.ts.bv.bar_edited();
                    cx.notify();
                }
                _ => {}
            })
            .detach();
            bp.bar = Some(bar);
        }
        if bp.take_focus_bar()
            && let Some(b) = &bp.bar
        {
            window.focus(&b.read(cx).focus_handle(cx), cx);
        }
        let focus = bp.focus.get_or_insert_with(|| cx.focus_handle());
        bp.captured = focus.is_focused(window);
        // the page's address in the bar, unless the person is typing in it
        if let (Some(bar), Some(p)) = (bp.bar.clone(), page)
            && p.url != bp.bar_url
            && !bar.read(cx).focus_handle(cx).is_focused(window)
        {
            bp.bar_url = p.url.clone();
            bar.update(cx, |s, cx| s.set_value(p.url, window, cx));
        }
    }

    /// A press on the page: the page takes the keys, the press goes in (only inside the page).
    fn browser_press(
        &mut self,
        e: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bp = &mut self.ts.bv;
        if let Some(f) = &bp.focus {
            window.focus(f, cx);
        }
        if let Some((x, y)) = bp.pointer((e.position.x.into(), e.position.y.into()), false) {
            bp.input(PageInput::Button {
                down: true,
                x,
                y,
                button: button_of(e.button),
                clicks: u8::try_from(e.click_count).unwrap_or(3),
                mods: 0,
            });
        }
        cx.notify();
    }

    /// A release, also one that ended outside the page (a drag), so the page never sees a button stuck down.
    fn browser_release(&mut self, e: &gpui::MouseUpEvent) {
        let bp = &mut self.ts.bv;
        if let Some((x, y)) = bp.pointer((e.position.x.into(), e.position.y.into()), true) {
            bp.input(PageInput::Button {
                down: false,
                x,
                y,
                button: button_of(e.button),
                clicks: u8::try_from(e.click_count).unwrap_or(3),
                mods: 0,
            });
        }
    }

    /// The window's key hook, for the page: `Some(consumed)` when the page holds the keys, `None` when it does not (or
    /// a dialog owns the keyboard) and for app chords. The URL bar and the chat input never get here with the page
    /// focused: focus is one place.
    pub(super) fn browser_key(
        &mut self,
        ks: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let held = self
            .ts
            .bv
            .focus
            .as_ref()
            .is_some_and(|f| f.is_focused(window));
        if !held
            || self
                .live
                .as_ref()
                .is_some_and(|l| !l.live.dialogs.is_empty())
        {
            return None;
        }
        if !self.shown || self.pane_tab() != ChatTab::Browser || !self.ts.bv.is_attached() {
            // the tab or view was left with the page still holding the keys: the prompt takes them back
            self.focus_prompt(window, cx);
            return None;
        }
        match page_key(ks, cfg!(target_os = "macos")) {
            PageKey::Pass => None,
            PageKey::Ignore => Some(true),
            PageKey::Release => {
                self.focus_prompt(window, cx);
                cx.notify();
                Some(true)
            }
            PageKey::Send(ev) => {
                self.ts.bv.input(ev);
                Some(true)
            }
            PageKey::NewTab => {
                self.browser_new_tab(cx);
                Some(true)
            }
            PageKey::CloseTab => {
                self.ts.bv.close_active();
                cx.notify();
                Some(true)
            }
            PageKey::CycleTab(back) => {
                self.ts.bv.cycle(back);
                cx.notify();
                Some(true)
            }
            PageKey::Paste => {
                // the clipboard is read here, on the person's own paste chord, and nowhere else; not while the
                // pane shows a refusal (the agent is using the tab, a password field): nothing would arrive
                if !self.ts.bv.has_note()
                    && let Some(t) = cx.read_from_clipboard().and_then(|c| c.text())
                {
                    self.ts.bv.paste(t);
                }
                Some(true)
            }
        }
    }

    /// A blank tab; the address bar takes the focus once the browser has really opened it (`take_focus_bar`).
    fn browser_new_tab(&mut self, cx: &mut Context<Self>) {
        self.ts.bv.act(Act::NewTab);
        cx.notify();
    }

    /// The URL bar holds the keys: the window-wide key hook leaves them alone.
    pub(super) fn bar_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.pane_tab() == ChatTab::Browser
            && self
                .ts
                .bv
                .bar
                .as_ref()
                .is_some_and(|b| b.read(cx).focus_handle(cx).is_focused(window))
    }

    /// While no browser runs: one line saying so, with the Start button, or why there is no button.
    pub(super) fn browser_start_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let access = self
            .live
            .as_ref()
            .and_then(|l| l.live.engine.as_ref())
            .and_then(|e| e.browser_access());
        let launch = self.ts.bv.launch_state();
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(12.5))
            .text_color(dim());
        row = match access {
            None => row.child(
                "No browser here: the Browser tab cannot start one while the session runs in WSL or remotely.",
            ),
            Some((tap, h)) => {
                let busy = launch == Launch::Starting;
                row.child("No browser is running. Start one to browse, or ask the agent to open a page.")
                    .child(
                        div()
                            .id("bv-start")
                            .px(px(10.))
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(hsla(t::BORDER))
                            .text_color(strong())
                            .when(!busy, |d| {
                                d.cursor_pointer()
                                    .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.08)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let tap = tap.clone();
                                        this.ts.bv.launch_with(
                                            &h,
                                            Box::pin(async move { tap.start().await }),
                                        );
                                        cx.notify();
                                    }))
                            })
                            .child(if busy { "Starting..." } else { "Start browser" }),
                    )
            }
        };
        let mut col = div().flex().flex_col().gap(px(6.)).child(row);
        if let Launch::Failed(e) = launch {
            col = col.child(
                div()
                    .text_size(px(12.5))
                    .text_color(hsla(t::ERROR))
                    .child(format!("Browser not started: {e}")),
            );
        }
        col.into_any_element()
    }

    /// The live page view, or `None` while no browser runs (the tab then keeps its earlier body).
    pub(super) fn browser_live(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let bp = &self.ts.bv;
        if !bp.is_attached() {
            return None;
        }
        let (frame, frame_err, page, note) = bp.snapshot();
        let motion = self.live.as_ref().is_none_or(|l| l.cfg.tui.motion);
        let now = crate::live::now_ms();
        let button = |id: &'static str, glyph: &'static str, act: Act| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.))
                .flex_none()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ts.bv.act(act.clone());
                    cx.notify();
                }))
                .child(icon(glyph, 15., dim()))
        };
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(30.))
            .child(button("bv-back", "arrow-left", Act::Back))
            .child(button("bv-fwd", "arrow-right", Act::Forward))
            .child(button("bv-reload", "rotate-cw", Act::Reload));
        bar = bar.child(
            div()
                .debug_selector(|| "browser-bar".to_string())
                .flex_1()
                .min_w(px(0.))
                .children(bp.bar.as_ref().map(Input::new)),
        );
        // the tab strip sits above the address row, like a browser's own
        let mut col = div().flex().flex_col().gap(px(8.)).size_full();
        if let Some(p) = page.as_ref() {
            let many = p.tabs.len() > 1;
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .overflow_hidden()
                    .children(p.tabs.iter().enumerate().map(|(n, title)| {
                        let i = p.offset + n; // the browser's own index
                        let on = i == p.active;
                        let id = p.ids.get(n).cloned().unwrap_or_default();
                        let id2 = id.clone();
                        div()
                            .id(("bv-tab", i))
                            .pl(px(10.))
                            .pr(px(4.))
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .max_w(px(180.))
                            .rounded(px(5.))
                            .text_size(px(12.))
                            .cursor_pointer()
                            .text_color(if on { strong() } else { dim() })
                            .when(on, |d| d.bg(hsla(t::ELEVATED)))
                            .when(!on, |d| {
                                d.hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.05)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ts.bv.act(Act::Tab(i, id.clone()));
                                cx.notify();
                            }))
                            .child(div().flex_1().min_w(px(0.)).truncate().child(title.clone()))
                            .when(many, |d| {
                                d.child(
                                    div()
                                        .id(("bv-tab-x", i))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(18.))
                                        .flex_none()
                                        .rounded(px(4.))
                                        .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.12)))
                                        // the click must not also select the tab behind it
                                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                            cx.stop_propagation()
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.ts.bv.act(Act::Close(i, id2.clone()));
                                            cx.notify();
                                        }))
                                        .child(icon("close", 12., dim())),
                                )
                            })
                    }))
                    .child(
                        div()
                            .id("bv-new-tab")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.))
                            .flex_none()
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.08)))
                            .on_click(cx.listener(|this, _, _, cx| this.browser_new_tab(cx)))
                            .child(icon("plus", 14., dim())),
                    ),
            );
        }
        col = col.child(bar);
        if let Some(r) = &bp.refusal {
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.5))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(hsla(t::ERROR))
                        .child(format!("Not opened: {}", r.text)),
                );
            if let Some(url) = r.open.clone() {
                row = row.child(
                    div()
                        .id("bv-open")
                        .px(px(8.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(hsla(t::BORDER))
                        .cursor_pointer()
                        .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.08)))
                        .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                        .child("Open in your browser"),
                );
            }
            col = col.child(row);
            // what the button hands to the person's own browser, whole (it is the typed address, short by rule)
            if let Some(url) = &r.open {
                col = col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(dim())
                        .child(format!("Opens in your browser: {url}")),
                );
            }
        } else if let Some(n) = note {
            col = col.child(
                div()
                    .text_size(px(12.5))
                    .text_color(hsla(t::WARN))
                    .child(sanitize_for_display(&n)),
            );
        }
        let shown = frame.is_some();
        let pointer = self
            .live
            .as_ref()
            .and_then(|l| l.live.view.panes.pointer.clone())
            .filter(|p| p.ts.is_some_and(|ts| panes::pointer_alpha(now - ts) > 0.0));
        let captured = bp.captured;
        let mut stage = div()
            .id("bv-stage")
            .debug_selector(|| "browser-stage".to_string())
            .relative()
            .flex_1()
            .min_h(px(120.))
            .w_full()
            .rounded(px(6.))
            .border_1()
            // the one accent border: only while the page takes the keys
            .border_color(hsla(if captured { t::ACCENT } else { t::BORDER }))
            .bg(hsla(t::BG))
            .overflow_hidden();
        if let Some(f) = &bp.focus {
            stage = stage.track_focus(f);
        }
        let stage = stage
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, e: &gpui::MouseDownEvent, w, cx| t.browser_press(e, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|t, e: &gpui::MouseDownEvent, w, cx| t.browser_press(e, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|t, e: &gpui::MouseDownEvent, w, cx| t.browser_press(e, w, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_up_out(
                MouseButton::Middle,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_up_out(
                MouseButton::Right,
                cx.listener(|t, e: &gpui::MouseUpEvent, _, _| t.browser_release(e)),
            )
            .on_mouse_move(cx.listener(|t, e: &gpui::MouseMoveEvent, _, _| {
                let held = |b| u8::from(e.pressed_button == Some(b));
                let buttons = held(MouseButton::Left)
                    | held(MouseButton::Right) << 1
                    | held(MouseButton::Middle) << 2;
                if let Some((x, y)) =
                    t.ts.bv
                        .pointer((e.position.x.into(), e.position.y.into()), false)
                {
                    t.ts.bv.input(PageInput::Move { x, y, buttons });
                }
            }))
            .on_scroll(cx.listener(|t, e: &gpui::ScrollEvent, _, _| {
                if let Some((x, y)) =
                    t.ts.bv
                        .pointer((e.position.x.into(), e.position.y.into()), false)
                {
                    let d = e.delta.pixel_delta(px(16.));
                    // GPUI: positive y moves the content down; the page scrolls the other way
                    let (dx, dy) = (-f64::from(f32::from(d.x)), -f64::from(f32::from(d.y)));
                    t.ts.bv.input(PageInput::Wheel {
                        x,
                        y,
                        dx,
                        dy,
                        mods: 0,
                    });
                }
            }));
        let fit = bp.fit.clone();
        let stage_at = bp.stage.clone();
        let frame_d = frame.clone();
        // The canvas is always there: it reports the pane's size (the page's viewport follows it) and paints the frame.
        let stage = stage.child(
            canvas(
                move |b, window, cx| {
                    let (w, h) = (
                        f64::from(f32::from(b.size.width)),
                        f64::from(f32::from(b.size.height)),
                    );
                    if let Ok(mut s) = stage_at.lock() {
                        *s = Some((
                            f32::from(b.origin.x),
                            f32::from(b.origin.y),
                            f32::from(b.size.width),
                            f32::from(b.size.height),
                        ));
                    }
                    let want = ViewMetrics::fit(w, h, f64::from(window.scale_factor()));
                    fit.send_if_modified(|c| (*c != want).then(|| *c = want).is_some());
                    frame_d
                        .as_ref()
                        .and_then(|d| prepare_page(b, d, pointer.as_ref(), now, motion, window, cx))
                },
                move |_, prepared, window, cx| {
                    if let (Some((at, cursor)), Some(d)) = (prepared, &frame) {
                        let _ = window.paint_image(
                            at,
                            at,
                            Corners::default(),
                            d.image.clone(),
                            0,
                            false,
                        );
                        if let Some(mut el) = cursor {
                            el.paint(window, cx);
                        }
                    }
                },
            )
            .absolute()
            .size_full(),
        );
        col = col.child(if shown {
            stage
        } else {
            stage
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim())
                .child(frame_err.map_or_else(
                    || "Waiting for the first frame...".to_string(),
                    |e| format!("Frame not shown: {e}"),
                ))
        });
        col = col.child(div().text_size(px(12.)).text_color(dim()).child(if captured {
            "Typing goes to the page. Esc gives the keys back. Paste sends your clipboard to the page."
        } else {
            "Click the page to click, scroll and type in it. The agent works in this browser too and can read the pages open in it."
        }));
        Some(
            div()
                .debug_selector(|| "tab-pane".to_string())
                .flex_1()
                .min_h(px(0.))
                .py(px(10.))
                .child(col)
                .into_any_element(),
        )
    }
}

/// Where the frame goes inside `b` (1:1 in device pixels), with the agent cursor laid out over it (layout is only
/// allowed while preparing, so the cursor is built here and painted later).
fn prepare_page(
    b: Bounds<gpui::Pixels>,
    d: &Decoded,
    pointer: Option<&synthesis_view::agent::Pointer>,
    now: i64,
    motion: bool,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Option<(Bounds<gpui::Pixels>, Option<AnyElement>)> {
    let (x, y, w, h) = place(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        d.w as f32,
        d.h as f32,
        window.scale_factor(),
    )?;
    let at = Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
    let cursor = pointer
        .zip(pointer.and_then(|p| panes::cursor_on(p, d.w, d.h)))
        .map(|(p, to)| {
            let (fx, fy) = panes::glide(p.from, to, now - p.ts.unwrap_or(now), motion);
            let alpha = panes::pointer_alpha(now - p.ts.unwrap_or(now));
            let mut el = div()
                .relative()
                .size(px(w))
                .h(px(h))
                .child(panes::cursor_overlay(fx, fy, alpha, now, motion, &p.action))
                .into_any_element();
            el.prepaint_as_root(
                at.origin,
                size(
                    AvailableSpace::Definite(px(w)),
                    AvailableSpace::Definite(px(h)),
                ),
                window,
                cx,
            );
            el
        });
    Some((at, cursor))
}

// ---- tests ------------------------------------------------------------------------------------------------

/// A browser that is not there: one fixed frame, and a record of what the pane asked of it.
#[cfg(any(test, feature = "shot"))]
pub struct FakeSource {
    pub jpeg: Vec<u8>,
    pub page: Mutex<PageState>,
    pub calls: Mutex<Vec<String>>,
    pub policy: UrlPolicy,
    /// How long `view` takes (to make calls overlap if they could), and (in flight now, most at once).
    pub view_delay: Mutex<Duration>,
    /// The next this-many `view(true)` calls fail.
    pub view_fails: std::sync::atomic::AtomicUsize,
    /// The page viewport of every `view(true)` call, in order.
    pub fits: Mutex<Vec<Option<ViewMetrics>>>,
    /// What `input` answers (`None`: accepted).
    pub input_err: Mutex<Option<String>>,
    /// How long one `input` call takes.
    pub input_delay: Mutex<Duration>,
    flight: Arc<(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    )>,
}

#[cfg(any(test, feature = "shot"))]
impl FakeSource {
    pub fn new(jpeg: Vec<u8>, page: PageState) -> Arc<Self> {
        Arc::new(Self {
            jpeg,
            page: Mutex::new(page),
            calls: Mutex::default(),
            policy: UrlPolicy::default(),
            view_delay: Mutex::default(),
            view_fails: Default::default(),
            fits: Mutex::default(),
            input_err: Mutex::default(),
            input_delay: Mutex::default(),
            flight: Arc::default(),
        })
    }

    #[cfg(test)]
    pub fn most_at_once(&self) -> usize {
        self.flight.1.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn log(&self, s: impl Into<String>) {
        self.calls.lock().unwrap().push(s.into());
    }
}

#[cfg(any(test, feature = "shot"))]
struct OneFrame(Option<Frame>);

#[cfg(any(test, feature = "shot"))]
impl Feed for OneFrame {
    fn next(&mut self) -> BoxFuture<'_, Option<Frame>> {
        Box::pin(async move {
            match self.0.take() {
                Some(f) => Some(f),
                None => std::future::pending().await, // an idle page sends nothing more
            }
        })
    }
}

#[cfg(any(test, feature = "shot"))]
impl Source for FakeSource {
    fn ready(&self) -> bool {
        true
    }
    fn check(&self, raw: &str) -> Result<String, String> {
        check_bar(&self.policy, raw)
    }
    fn view(&self, on: bool, fit: Option<ViewMetrics>) -> BoxFuture<'static, Result<(), String>> {
        use std::sync::atomic::Ordering::SeqCst;
        self.log(format!("view({on})"));
        if on {
            self.fits.lock().unwrap().push(fit);
        }
        // one caller at a time in the tests, so load then subtract is enough
        let fail =
            on && self.view_fails.load(SeqCst) > 0 && self.view_fails.fetch_sub(1, SeqCst) > 0;
        let (delay, flight) = (*self.view_delay.lock().unwrap(), self.flight.clone());
        Box::pin(async move {
            let now = flight.0.fetch_add(1, SeqCst) + 1;
            flight.1.fetch_max(now, SeqCst);
            tokio::time::sleep(delay).await;
            flight.0.fetch_sub(1, SeqCst);
            if fail {
                Err("the browser is busy".to_string())
            } else {
                Ok(())
            }
        })
    }
    fn frames(&self) -> Option<Box<dyn Feed>> {
        let f = Frame {
            seq: 1,
            jpeg: self.jpeg.clone(),
            meta: Default::default(),
        };
        Some(Box::new(OneFrame(Some(f))))
    }
    fn page(&self) -> BoxFuture<'static, Result<PageState, String>> {
        let p = self.page.lock().unwrap().clone();
        Box::pin(async { Ok(p) })
    }
    fn act(&self, a: Act) -> BoxFuture<'static, Result<(), String>> {
        self.log(format!("{a:?}"));
        let mut p = self.page.lock().unwrap();
        match a {
            Act::Tab(i, _) => p.active = i,
            Act::NewTab => {
                p.tabs.push("New tab".into());
                p.ids.push("new".into());
                p.active = p.tabs.len() - 1;
            }
            Act::Close(i, _) => {
                p.tabs.remove(i);
                p.ids.remove(i);
                p.active = p.active.min(p.tabs.len() - 1);
            }
            _ => {}
        }
        drop(p);
        Box::pin(async { Ok(()) })
    }
    fn input(&self, ev: PageInput) -> BoxFuture<'static, Result<(), String>> {
        self.log(format!("input {ev:?}"));
        let (err, delay) = (
            self.input_err.lock().unwrap().clone(),
            *self.input_delay.lock().unwrap(),
        );
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            err.map_or(Ok(()), Err)
        })
    }
}

/// A JPEG of `w` x `h` with a gradient (the fixed frame of the shots and the tests).
#[cfg(any(test, feature = "shot"))]
pub fn sample_jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 150])
    });
    let mut out = vec![];
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80)
        .encode_image(&img)
        .expect("encode");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_is_drawn_one_to_one_on_a_device_pixel() {
        // 2x: a 1800x1200 px frame is 900x600 logical; the origin is snapped to a half logical pixel
        assert_eq!(
            place(10., 20., 1800., 1200., 2.),
            Some((10., 20., 900., 600.))
        );
        assert_eq!(
            place(10.3, 20.2, 1800., 1200., 2.),
            Some((10.5, 20., 900., 600.))
        );
        // 1x, and a fractional scale (1.5: 3 device pixels are 2 logical)
        assert_eq!(place(0., 0., 640., 400., 1.), Some((0., 0., 640., 400.)));
        assert_eq!(place(0., 0., 150., 150., 1.5), Some((0., 0., 100., 100.)));
        // never scaled to the pane: a frame bigger than the pane keeps its size (the pane clips it)
        assert_eq!(place(0., 0., 4000., 100., 2.), Some((0., 0., 2000., 50.)));
        // no size, no frame
        assert_eq!(place(0., 0., 0., 800., 2.), None);
        assert_eq!(place(0., 0., 800., 800., 0.), None);
        assert_eq!(place(f32::NAN, 0., 10., 10., 1.), None);
    }

    #[test]
    fn the_page_is_resized_once_after_the_pane_size_holds_still() {
        let page = PageState::default();
        let fake = FakeSource::new(sample_jpeg(16, 16), page);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        bp.sync(true);
        wait("the view on", || fake.calls() == ["view(true)"]);
        let m = |w| ViewMetrics::fit(w, 600., 2.);
        // before the pane reported a size: the browser's own viewport
        assert_eq!(*fake.fits.lock().unwrap(), [None]);
        // a drag: many sizes in a row are one resize, to the last
        for w in [800., 810., 820., 830.] {
            bp.fit.send_replace(m(w));
            std::thread::sleep(Duration::from_millis(10)); // far inside the debounce, even on a loaded runner
        }
        wait("the resize", || fake.fits.lock().unwrap().len() >= 2);
        std::thread::sleep(RESIZE_DEBOUNCE * 2);
        assert_eq!(*fake.fits.lock().unwrap(), [None, m(830.)]);
        // the same size again is no resize; a hide and show starts with the size the pane has
        // as the drawing code reports it: an equal size does not notify
        bp.fit
            .send_if_modified(|c| (*c != m(830.)).then(|| *c = m(830.)).is_some());
        bp.sync(false);
        wait("view(false)", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
        });
        bp.sync(true);
        wait("view(true) again", || fake.fits.lock().unwrap().len() == 3);
        assert_eq!(fake.fits.lock().unwrap()[2], m(830.));
    }

    #[test]
    fn the_url_bar_refuses_what_the_browser_policy_refuses() {
        let p = UrlPolicy::default();
        for u in [
            "file:///etc/passwd",
            "http://169.254.169.254",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://192.168.0.1:3000",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "ftp://example.com/",
            "  ",
        ] {
            assert!(check_bar(&p, u).is_err(), "{u} must be refused");
        }
        // the person may open loopback (the agent may not: `check_navigation` stays strict)
        for u in [
            "http://localhost:3000",
            "localhost:3000",
            "http://[::1]:3000/",
            "127.0.0.1:8080",
        ] {
            assert!(check_bar(&p, u).is_ok(), "{u} must be allowed");
        }
        assert!(p.check_navigation("http://localhost:3000").is_err());
        assert_eq!(
            check_bar(&p, "https://example.com/a").unwrap(),
            "https://example.com/a"
        );
        // no scheme: https
        assert_eq!(check_bar(&p, "example.com").unwrap(), "https://example.com");
        // a deny list applies to the bar too
        let deny = UrlPolicy {
            deny_domains: vec!["example.org".into()],
            ..UrlPolicy::default()
        };
        assert!(check_bar(&deny, "https://www.example.org").is_err());
    }

    #[test]
    fn open_in_your_browser_is_for_typed_http_addresses_only() {
        let r = refusal("private".into(), "http://10.0.0.1");
        assert_eq!(r.open.as_deref(), Some("http://10.0.0.1"));
        assert!(
            refusal("scheme".into(), "file:///etc/passwd")
                .open
                .is_none()
        );
        assert!(
            refusal("scheme".into(), "javascript:alert(1)")
                .open
                .is_none()
        );
        assert!(refusal("scheme".into(), "data:text/html,x").open.is_none());
    }

    #[test]
    fn a_frame_over_the_cap_is_refused_before_decoding() {
        assert!(decode(&sample_jpeg(64, 48)).is_ok());
        assert!(decode(&sample_jpeg(MAX_SIDE, 8)).is_ok());
        let e = decode(&sample_jpeg(MAX_SIDE + 1, 8)).unwrap_err();
        assert!(e.contains("refused"), "{e}");
        assert!(decode(&sample_jpeg(8, MAX_SIDE + 1)).is_err());
        assert!(decode(b"not a jpeg").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn a_frame_never_prints_its_pixels() {
        let f = Frame {
            seq: 1,
            jpeg: vec![0xAB; 64],
            meta: Default::default(),
        };
        let s = format!("{f:?}");
        assert!(!s.contains("171") && !s.contains("0xAB"), "{s}");
        let d = to_decoded(decode(&sample_jpeg(8, 8)).unwrap());
        assert_eq!(format!("{d:?}"), "Decoded(8x8)");
    }

    fn wait(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn tab_titles_lose_bidi_and_invisible_controls() {
        let s = sanitize_for_display("evil\u{202e}gpj.exe\u{200b}\u{2066}x");
        assert!(
            !s.chars()
                .any(|c| matches!(c, '\u{202e}' | '\u{200b}' | '\u{2066}')),
            "{s}"
        );
    }

    #[test]
    fn a_tab_action_carries_the_id_the_person_saw() {
        let p = PageState {
            tabs: vec!["a".into(), "b".into()],
            ids: vec!["ia".into(), "ib".into()],
            active: 1,
            ..Default::default()
        };
        // Ctrl+Tab from the last wraps to the first: index and id of THAT tab travel together
        assert_eq!(next_tab(&p, false), Some(Act::Tab(0, "ia".into())));
    }

    #[test]
    fn tab_chords_and_cycling() {
        let k = |key: &str, mods: &str| ks(key, None, mods);
        assert_eq!(page_key(&k("t", "m"), true), PageKey::NewTab);
        assert_eq!(page_key(&k("w", "m"), true), PageKey::CloseTab);
        assert_eq!(page_key(&k("t", "c"), false), PageKey::NewTab);
        // macOS: Ctrl+T / Ctrl+C are not the tabs' (Ctrl+C interrupts the agent)
        assert_eq!(page_key(&k("c", "c"), true), PageKey::Pass);
        assert_ne!(page_key(&k("t", "c"), true), PageKey::NewTab);
        assert_eq!(page_key(&k("tab", "c"), true), PageKey::CycleTab(false));
        assert_eq!(page_key(&k("tab", "cs"), false), PageKey::CycleTab(true));
        // Esc still gives the keys back
        assert_eq!(page_key(&k("escape", "c"), true), PageKey::Release);
        let p = PageState {
            tabs: vec!["a".into(), "b".into(), "c".into()],
            ids: vec!["ia".into(), "ib".into(), "ic".into()],
            active: 5,
            offset: 3,
            ..Default::default()
        };
        assert_eq!(
            (next_tab(&p, false), next_tab(&p, true)),
            (
                Some(Act::Tab(3, "ia".into())),
                Some(Act::Tab(4, "ib".into()))
            )
        );
        let one = PageState {
            tabs: vec!["a".into()],
            ..Default::default()
        };
        assert_eq!(next_tab(&one, false), None);
    }

    #[test]
    fn the_view_runs_only_while_the_pane_is_shown() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = PageState {
            url: "https://a.test/".into(),
            title: "A".into(),
            tabs: vec!["A".into(), "B".into()],
            ids: vec!["ia".into(), "ib".into()],
            active: 0,
            offset: 0,
        };
        let fake = FakeSource::new(sample_jpeg(64, 40), page);
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        // attached but hidden: nothing asked of the browser
        bp.sync(false);
        assert!(fake.calls().is_empty());
        // shown: view(true), a decoded frame arrives
        bp.sync(true);
        wait("the first frame", || bp.snapshot().0.is_some());
        assert_eq!(fake.calls(), ["view(true)"]);
        let (f, _, p, _) = bp.snapshot();
        assert_eq!((f.unwrap().w, p.is_some()), (64, true));
        // shown again: no second view(true)
        bp.sync(true);
        assert_eq!(fake.calls(), ["view(true)"]);
        // a tab switch re-points the view
        bp.act(Act::Tab(1, "ib".into()));
        wait("the re-target", || fake.calls().len() == 3);
        assert_eq!(fake.calls(), ["view(true)", "Tab(1, \"ib\")", "view(true)"]);
        // a new tab and a close re-point it too
        bp.act(Act::NewTab);
        wait("the re-target", || fake.calls().len() == 5);
        assert_eq!(bp.snapshot().2.unwrap().tabs.len(), 3);
        bp.close_active();
        wait("the re-target", || fake.calls().len() == 7);
        assert_eq!(
            &fake.calls()[3..],
            ["NewTab", "view(true)", "Close(2, \"new\")", "view(true)"]
        );
        // hidden (another tab, another session): view(false) and no pixels held
        bp.sync(false);
        wait("view(false)", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
        });
        assert!(bp.snapshot().0.is_none());
        // dropping a shown pane turns the view off too
        bp.sync(true);
        wait("view(true) again", || {
            fake.calls().iter().filter(|c| *c == "view(true)").count() == 5
        });
        drop(bp);
        wait("view(false) on drop", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
        });
    }

    #[test]
    fn the_pointer_maps_one_to_one_into_the_page() {
        let stage = (100.5, 40., 900.7, 600.2); // the viewport is the floor of the pane: 900 x 600
        assert_eq!(to_page((100.5, 40.), stage, false), Some((0., 0.)));
        assert_eq!(to_page((400.5, 240.), stage, false), Some((300., 200.)));
        // outside: nothing, except the release of a drag, which lands on the nearest edge
        assert_eq!(to_page((100., 100.), stage, false), None);
        assert_eq!(to_page((100.5 + 900., 100.), stage, false), None);
        assert_eq!(to_page((5000., -9.), stage, true), Some((899., 0.)));
        // nothing for a pane that has no size or a position that is not a number
        assert_eq!(to_page((1., 1.), (0., 0., 0., 10.), true), None);
        assert_eq!(to_page((f32::NAN, 1.), stage, true), None);
    }

    fn ks(key: &str, ch: Option<&str>, mods: &str) -> Keystroke {
        let mut k = Keystroke {
            key: key.into(),
            key_char: ch.map(Into::into),
            ..Default::default()
        };
        k.modifiers.control = mods.contains('c');
        k.modifiers.platform = mods.contains('m');
        k.modifiers.shift = mods.contains('s');
        k.modifiers.alt = mods.contains('a');
        k
    }

    fn key(k: &str, mods: u8) -> PageKey {
        PageKey::Send(PageInput::Key {
            key: k.into(),
            mods,
        })
    }

    #[test]
    fn keys_map_with_modifiers_special_keys_and_app_chords() {
        // typing: the typed character, shift included; outside ASCII it is text
        assert_eq!(page_key(&ks("a", Some("a"), ""), true), key("a", 0));
        assert_eq!(page_key(&ks("a", Some("A"), "s"), true), key("A", SHIFT));
        assert_eq!(page_key(&ks("space", Some(" "), ""), true), key(" ", 0));
        assert_eq!(
            page_key(&ks("e", Some("\u{e9}"), "a"), true),
            PageKey::Send(PageInput::Text("\u{e9}".into()))
        );
        // special keys keep their modifiers
        assert_eq!(page_key(&ks("enter", None, ""), true), key("Enter", 0));
        assert_eq!(page_key(&ks("tab", None, "s"), true), key("Tab", SHIFT));
        assert_eq!(
            page_key(&ks("left", None, "a"), true),
            key("ArrowLeft", ALT)
        );
        assert_eq!(
            page_key(&ks("pagedown", None, ""), true),
            key("PageDown", 0)
        );
        // Esc gives the keys back (Ctrl+Esc too); a key with no mapping is swallowed
        assert_eq!(page_key(&ks("escape", None, ""), true), PageKey::Release);
        assert_eq!(page_key(&ks("escape", None, "c"), false), PageKey::Release);
        assert_eq!(page_key(&ks("f5", None, ""), true), PageKey::Ignore);
        // shortcuts: Cmd on macOS, Ctrl elsewhere; only select all, undo, redo and paste are the page's
        assert_eq!(page_key(&ks("a", None, "m"), true), key("a", META));
        assert_eq!(page_key(&ks("z", None, "ms"), true), key("z", META | SHIFT));
        assert_eq!(page_key(&ks("v", None, "m"), true), PageKey::Paste);
        assert_eq!(page_key(&ks("v", None, "c"), false), PageKey::Paste);
        for (k, m, mac) in [
            ("1", "m", true),
            (",", "m", true),
            ("c", "m", true),
            ("1", "c", false),
            ("b", "c", false),
        ] {
            assert_eq!(page_key(&ks(k, None, m), mac), PageKey::Pass, "{k} {m}");
        }
        // a Ctrl chord on macOS (not the shortcut key) is the page's, without text, except Ctrl+C (interrupts the agent)
        assert_eq!(page_key(&ks("d", None, "c"), true), key("d", CTRL));
        assert_eq!(page_key(&ks("c", None, "c"), true), PageKey::Pass);
        // Ctrl+Cmd or Cmd+Option typing never turns into text
        assert_eq!(
            page_key(&ks("x", Some("\u{2248}"), "m"), false),
            key("x", META)
        );
    }

    fn mv(x: f64) -> PageInput {
        PageInput::Move {
            x,
            y: 0.,
            buttons: 0,
        }
    }

    #[test]
    fn a_flood_of_pointer_events_is_merged_and_bounded() {
        let mut q = InputQueue::default();
        assert!(q.push(mv(0.)), "the first starts the sender");
        for i in 1..1000 {
            assert!(!q.push(mv(f64::from(i))));
        }
        assert_eq!(q.q, [mv(999.)], "latest wins");
        let w = |dy| PageInput::Wheel {
            x: 1.,
            y: 1.,
            dx: 0.,
            dy,
            mods: 0,
        };
        q.push(w(3.));
        q.push(w(4.));
        assert_eq!(q.q.len(), 2);
        assert_eq!(q.q[1], w(7.), "wheel turns add up");
        // presses and keys keep their order and are bounded
        let k = |n: usize| PageInput::Key {
            key: n.to_string(),
            mods: 0,
        };
        for n in 0..500 {
            q.push(k(n));
        }
        assert_eq!(q.q.len(), MAX_PENDING);
        assert!(
            !q.q.iter()
                .any(|e| matches!(e, PageInput::Move { .. } | PageInput::Wheel { .. }))
        );
        // drained: the next push starts a sender again
        while q.pop().is_some() {}
        assert!(q.push(mv(1.)));
    }

    #[test]
    fn input_reaches_the_source_and_a_refusal_is_shown_and_clears() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = FakeSource::new(sample_jpeg(8, 8), PageState::default());
        let mut bp = BrowserPane::default();
        bp.input(mv(1.)); // not attached: nothing, no panic
        bp.attach(fake.clone(), rt.handle().clone());
        *fake.input_err.lock().unwrap() = Some(BrowserError::Busy.to_string());
        bp.input(mv(2.));
        wait("the refusal", || bp.snapshot().3.is_some());
        assert_eq!(
            bp.snapshot().3.as_deref(),
            Some("the agent is using this tab")
        );
        *fake.input_err.lock().unwrap() = None;
        bp.input(mv(3.));
        wait("the status to clear", || bp.snapshot().3.is_none());
        assert_eq!(fake.calls().len(), 2);
        // a paste over the limit is not sent and says why; one within it goes
        bp.paste("x".repeat(synthesis_browser::MAX_TEXT + 1));
        assert!(bp.has_note());
        assert_eq!(fake.calls().len(), 2);
        bp.paste("ok".into());
        wait("the paste", || {
            fake.calls().iter().any(|c| c.contains("Text(<2 chars>)"))
        });
    }

    #[test]
    fn a_refused_address_is_shown_and_never_sent() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = FakeSource::new(sample_jpeg(8, 8), PageState::default());
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        for u in [
            "file:///etc/passwd",
            "http://169.254.169.254",
            "http://10.0.0.1",
        ] {
            bp.go(u);
            assert!(bp.refusal.is_some(), "{u}");
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        bp.go("https://example.com");
        assert!(bp.refusal.is_none());
        wait("the navigation", || !fake.calls().is_empty());
        assert_eq!(fake.calls(), ["Go(\"https://example.com\")"]);
    }

    #[test]
    fn what_a_page_says_about_itself_is_bounded() {
        let long = "\u{e9}".repeat(10_000); // multi-byte: a byte cut would panic
        assert_eq!(clip(&long, MAX_TEXT).chars().count(), MAX_TEXT);
        let p = capped(PageState {
            url: long.clone(),
            title: long.clone(),
            tabs: vec![long; 100],
            ids: (0..100).map(|i| format!("id{i}")).collect(),
            active: 0,
            offset: 0,
        });
        assert_eq!(p.url.chars().count(), MAX_TEXT);
        assert_eq!(p.title.chars().count(), MAX_TEXT);
        assert_eq!(p.tabs.len(), MAX_TABS);
        assert!(p.tabs.iter().all(|t| t.chars().count() == MAX_TEXT));
    }

    #[test]
    fn a_huge_page_url_and_a_huge_typed_address_are_bounded() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let page = PageState {
            url: format!("https://a.test/{}", "x".repeat(50_000)),
            title: "t".repeat(50_000),
            tabs: vec!["a".into(), "b".repeat(50_000)],
            ids: vec![],
            active: 0,
            offset: 0,
        };
        let fake = FakeSource::new(sample_jpeg(8, 8), page);
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        bp.sync(true);
        wait("the page", || bp.snapshot().2.is_some());
        let p = bp.snapshot().2.unwrap();
        assert!(p.url.chars().count() <= MAX_TEXT && p.title.chars().count() <= MAX_TEXT);
        assert!(p.tabs.iter().all(|t| t.chars().count() <= MAX_TEXT));
        // typed: refused at Enter, never sent
        let before = fake.calls().len();
        bp.go(&format!("https://a.test/{}", "y".repeat(MAX_TYPED)));
        assert!(bp.refusal.as_ref().is_some_and(|r| r.open.is_none()));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.calls().len(), before);
    }

    #[test]
    fn quick_hide_and_show_ends_with_the_last_wish_and_calls_never_overlap() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let fake = FakeSource::new(sample_jpeg(16, 16), PageState::default());
        *fake.view_delay.lock().unwrap() = Duration::from_millis(40);
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        for want in [true, false, true, false, true] {
            bp.sync(want);
        }
        wait("the view on and a frame", || bp.snapshot().0.is_some());
        assert_eq!(fake.calls().last().map(String::as_str), Some("view(true)"));
        for want in [false, true, false] {
            bp.sync(want);
        }
        wait("the view off", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
        });
        std::thread::sleep(Duration::from_millis(150)); // nothing may follow it
        assert_eq!(fake.calls().last().map(String::as_str), Some("view(false)"));
        assert!(bp.snapshot().0.is_none());
        assert_eq!(
            fake.most_at_once(),
            1,
            "view calls overlapped: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn an_unedited_bar_does_not_navigate_to_the_redacted_address() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let fake = FakeSource::new(sample_jpeg(8, 8), PageState::default());
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        // the bar shows the redacted form of what the page is on
        bp.bar_url = "https://a.test/cb?token=%5Bredacted%5D".into();
        bp.go("https://a.test/cb?token=%5Bredacted%5D");
        std::thread::sleep(Duration::from_millis(50));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // an edited one does
        bp.go("https://b.test/");
        wait("the navigation", || !fake.calls().is_empty());
    }

    #[test]
    fn the_open_button_always_matches_the_displayed_refusal() {
        // a long address is refused without the button (it could not be shown whole)
        let long = format!("http://10.0.0.1/{}", "a".repeat(MAX_OPEN));
        assert!(refusal("private".into(), &long).open.is_none());
        let mut bp = BrowserPane::default();
        bp.refusal = Some(refusal("private".into(), "http://10.0.0.1"));
        assert!(bp.refusal.is_some());
        // editing the bar drops the refusal and with it the button
        bp.bar_edited();
        assert!(bp.refusal.is_none());
    }

    #[test]
    fn no_open_button_for_an_address_that_is_not_what_it_looks_like() {
        for u in [
            "http://10.0.0.1/\u{202e}gpj.exe",
            "http://10.0.0.1/a\u{200b}b",
            "http://10.0.0.1/\u{2066}x",
            "http://10.0.0.\u{ff11}",
            "http://b\u{fc}cher.example",
        ] {
            assert!(refusal("private".into(), u).open.is_none(), "{u:?}");
        }
        assert!(
            refusal("private".into(), "http://10.0.0.1/a-b?c=d")
                .open
                .is_some()
        );
    }

    #[test]
    fn a_failed_start_is_tried_again_without_hiding_and_showing() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let fake = FakeSource::new(sample_jpeg(16, 16), PageState::default());
        fake.view_fails
            .store(1, std::sync::atomic::Ordering::SeqCst);
        // the note lives only until the retry succeeds (START_RETRY + this delay): long enough to be seen on a slow runner
        *fake.view_delay.lock().unwrap() = Duration::from_millis(400);
        let mut bp = BrowserPane::default();
        bp.attach(fake.clone(), rt.handle().clone());
        bp.sync(true);
        wait("the note of the failed start", || bp.snapshot().3.is_some());
        assert!(bp.snapshot().3.unwrap().contains("trying again"));
        wait("the retry to show a frame", || bp.snapshot().0.is_some());
        assert_eq!(fake.calls(), ["view(true)", "view(true)"]);
        assert!(bp.snapshot().3.is_none(), "the note stays after recovery");
        // it gives up after MAX_STARTS and says how to retry
        let dead = FakeSource::new(sample_jpeg(16, 16), PageState::default());
        dead.view_fails
            .store(100, std::sync::atomic::Ordering::SeqCst);
        let mut bp = BrowserPane::default();
        bp.attach(dead.clone(), rt.handle().clone());
        bp.sync(true);
        wait("giving up", || {
            bp.snapshot().3.is_some_and(|n| n.contains("giving up"))
        });
        assert_eq!(dead.calls().len(), MAX_STARTS as usize);
    }

    #[test]
    fn the_active_tab_stays_in_the_strip_and_the_signature_sees_every_tab() {
        let tabs: Vec<String> = (0..40).map(|i| format!("t{i}")).collect();
        let at = |active| {
            capped(PageState {
                tabs: tabs.clone(),
                active,
                ..Default::default()
            })
        };
        let p = at(35);
        assert_eq!(p.tabs.len(), MAX_TABS);
        assert!(p.offset <= 35 && 35 < p.offset + MAX_TABS);
        assert_eq!(p.tabs[35 - p.offset], "t35");
        let p = at(39);
        assert_eq!((p.offset, p.tabs[39 - p.offset].as_str()), (8, "t39"));
        assert_eq!(at(3).offset, 0);
        // the poll compares the browser's own count and active tab: a 33rd tab changes it although the strip is full
        let (a, b) = (
            PageState {
                tabs: tabs[..32].to_vec(),
                ..Default::default()
            },
            PageState {
                tabs: tabs[..33].to_vec(),
                ..Default::default()
            },
        );
        assert_ne!((a.active, a.tabs.len()), (b.active, b.tabs.len()));
    }

    #[test]
    fn start_browser_shows_progress_then_the_error_or_clears() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut bp = BrowserPane::default();
        assert!(
            !bp.is_attached(),
            "unattached: the tab shows the empty state"
        );
        assert_eq!(bp.launch_state(), Launch::Idle);
        // a failure is kept for the pane, with the SYNT_BROWSER_PATH hint the launcher gave
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        bp.launch_with(rt.handle(), Box::pin(async move { rx.await.unwrap() }));
        assert_eq!(bp.launch_state(), Launch::Starting);
        // a second press while starting does nothing
        bp.launch_with(rt.handle(), Box::pin(async { Ok(()) }));
        tx.send(Err(
            "no Chrome found. Install one or set SYNT_BROWSER_PATH".into()
        ))
        .unwrap();
        wait("the failure", || {
            matches!(bp.launch_state(), Launch::Failed(_))
        });
        assert!(matches!(bp.launch_state(), Launch::Failed(e) if e.contains("SYNT_BROWSER_PATH")));
        // pressing again clears it; on success the state is idle and the tap's browser attaches via `ensure`
        bp.launch_with(rt.handle(), Box::pin(async { Ok(()) }));
        wait("success", || bp.launch_state() == Launch::Idle);
        let fake = FakeSource::new(sample_jpeg(16, 16), PageState::default());
        bp.attach(fake, rt.handle().clone());
        assert!(bp.is_attached());
    }

    #[test]
    fn a_tap_without_a_launcher_refuses_to_start() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let tap = BrowserTap::default();
        assert!(!tap.can_start());
        assert!(
            rt.block_on(tap.start())
                .unwrap_err()
                .contains("cannot start")
        );
    }
}
