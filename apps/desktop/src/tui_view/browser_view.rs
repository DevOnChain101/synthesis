//! The Browser tab as a page view (spec `docs/specs/terminal-browser.md` 2.3, 2.6, PR B2, VIEW ONLY): the live
//! frames of the session's browser (`BrowserRuntime::view` / `frames`) drawn letterboxed into the pane, a URL bar
//! whose navigation passes the browser's own `UrlPolicy`, back / forward / reload, the browser's tabs and the
//! agent cursor. Nothing the person does in the pane reaches the page (input is B3); the pane says so.
//!
//! Frames are the person's own view: they bypass `guard_capture`, and they never go anywhere but the pane's one
//! decoded slot (never an event, the store, a log or the model). Decoding runs off the UI thread, the frame
//! size is capped before the decode and only the newest frame is kept.
//!
//! The pane starts nothing: it shows the browser once the agent has launched one (`BrowserTap`), else the tab keeps
//! its earlier look (the saved screenshots and the call list).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{Receiver, Sender, channel};
use futures::future::BoxFuture;
use gpui::{
    AnyElement, AvailableSpace, Bounds, Context, Corners, Entity, Focusable as _, RenderImage,
    Window, canvas, div, point, prelude::*, px, size,
};
use gpui_component::input::{Input, InputEvent, InputState};
use synthesis_browser::{BrowserError, Frame, UrlPolicy};
use synthesis_cli_ui_render::theme as t;
use synthesis_harness::sanitize_for_display;
use synthesis_tools::BrowserTap;
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use super::{ChatTab, TuiView};
use crate::theme::hsla;
use crate::widgets::chrome::{dim, icon, strong, with_alpha};
use crate::widgets::panes;

/// Largest side of a frame the pane decodes (spec 2.3); a bigger one is refused before the decode.
pub const MAX_SIDE: u32 = 4096;
/// The decoder may not allocate more than this for one frame (4096 x 4096 RGBA is 64 MiB).
const MAX_ALLOC: u64 = 80 << 20;
/// The size Chrome is asked to cast at (it scales down to it, keeping the aspect ratio).
const VIEW_W: u32 = 1600;
const VIEW_H: u32 = 1000;
/// How often the pane asks the browser for its URL, title and tabs.
const POLL: Duration = Duration::from_secs(1);

// ---- pure parts -------------------------------------------------------------------------------------------

/// Where a frame of `fw` x `fh` is drawn inside a pane of `pw` x `ph`: as large as fits, centred, aspect ratio kept
/// (the bars at the sides or top and bottom are the letterbox). `None` for a size that is not a positive finite number.
pub fn letterbox(pw: f32, ph: f32, fw: f32, fh: f32) -> Option<(f32, f32, f32, f32)> {
    let ok = |v: f32| v.is_finite() && v > 0.0;
    if !(ok(pw) && ok(ph) && ok(fw) && ok(fh)) {
        return None;
    }
    let k = (pw / fw).min(ph / fh);
    let (w, h) = (fw * k, fh * k);
    Some(((pw - w) / 2.0, (ph - h) / 2.0, w, h))
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
/// only, no private, link-local or metadata address unless the user allowed it, `deny_domains`...).
pub fn check_bar(policy: &UrlPolicy, raw: &str) -> Result<String, String> {
    let url = bar_address(raw)?;
    policy.check_navigation(&url)?;
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
        (l.starts_with("http://") || l.starts_with("https://")) && !l.contains(char::is_whitespace)
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
    pub active: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Go(String),
    Back,
    Forward,
    Reload,
    Tab(usize),
}

pub trait Feed: Send {
    /// The newest frame (the browser keeps one slot), `None` when the browser is gone.
    fn next(&mut self) -> BoxFuture<'_, Option<Frame>>;
}

/// The browser the pane looks at (a fake in the tests and the shots).
pub trait Source: Send + Sync + 'static {
    fn ready(&self) -> bool;
    fn check(&self, raw: &str) -> Result<String, String>;
    fn view(&self, on: bool) -> BoxFuture<'static, Result<(), String>>;
    fn frames(&self) -> Option<Box<dyn Feed>>;
    fn page(&self) -> BoxFuture<'static, Result<PageState, String>>;
    fn act(&self, a: Act) -> BoxFuture<'static, Result<(), String>>;
}

/// The real thing: the runtime the agent launched, found through the tap (weakly: the pane never keeps Chrome alive).
pub struct RuntimeSource(pub Arc<BrowserTap>);

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
        self.0.runtime().is_some()
    }

    fn check(&self, raw: &str) -> Result<String, String> {
        let rt = self.0.runtime().ok_or("no browser is running")?;
        check_bar(rt.policy(), raw)
    }

    fn view(&self, on: bool) -> BoxFuture<'static, Result<(), String>> {
        let rt = self.0.runtime();
        Box::pin(async move {
            match rt {
                Some(rt) => rt.view(on, VIEW_W, VIEW_H).await.map_err(why),
                None => Ok(()),
            }
        })
    }

    fn frames(&self) -> Option<Box<dyn Feed>> {
        Some(Box::new(Stream(self.0.runtime()?.frames())))
    }

    fn page(&self) -> BoxFuture<'static, Result<PageState, String>> {
        let rt = self.0.runtime();
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
            })
        })
    }

    fn act(&self, a: Act) -> BoxFuture<'static, Result<(), String>> {
        let rt = self.0.runtime();
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
                Act::Tab(i) => rt.select_tab(i).await,
            }
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
}

struct Link {
    src: Arc<dyn Source>,
    handle: Handle,
    /// Which tap it came from (0: a fixed source of a test or a shot, never replaced).
    tap: usize,
    /// `view(true)` is in force.
    on: bool,
    tasks: Vec<AbortHandle>,
}

pub struct BrowserPane {
    link: Option<Link>,
    shared: Arc<Mutex<Shared>>,
    wake: Sender<()>,
    /// Taken once by the window, which repaints on every `()`.
    pub wake_rx: Option<Receiver<()>>,
    pub bar: Option<Entity<InputState>>,
    bar_url: String,
    pub refusal: Option<Refusal>,
    /// The image last handed to the window, to free it from the atlas when the next one comes.
    painted: Option<Arc<RenderImage>>,
}

impl Default for BrowserPane {
    fn default() -> Self {
        let (wake, rx) = channel(1);
        Self {
            link: None,
            shared: Arc::default(),
            wake,
            wake_rx: Some(rx),
            bar: None,
            bar_url: String::new(),
            refusal: None,
            painted: None,
        }
    }
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        self.sync(false);
    }
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
        self.link = Some(Link {
            src,
            handle,
            tap: 0,
            on: false,
            tasks: vec![],
        });
    }

    /// Follows the session's browser: attached once the agent has launched it, dropped when it is gone.
    pub fn ensure(&mut self, tap: &Arc<BrowserTap>, handle: &Handle) {
        let id = Arc::as_ptr(tap) as usize;
        let ready = tap.runtime().is_some();
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

    /// The pane is shown (`want`) or not: starts the view of the browser (`view(true)`, the frame pump and the
    /// page poll) or stops it (`view(false)`, nothing decoded or held any more). Cheap when nothing changes.
    pub fn sync(&mut self, want: bool) {
        let Some(l) = &mut self.link else { return };
        if want == l.on {
            return;
        }
        l.on = want;
        for t in l.tasks.drain(..) {
            t.abort();
        }
        let (src, shared) = (l.src.clone(), self.shared.clone());
        if !want {
            lock(&shared).frame = None; // a hidden pane holds no pixels
            // `view(false)` also when the browser is mid-start: a cancelled start leaves the view off
            l.handle.spawn(async move {
                let _ = src.view(false).await;
            });
            return;
        }
        let wake = self.wake.clone();
        l.tasks.push(
            l.handle
                .spawn(start(src.clone(), shared.clone(), wake.clone()))
                .abort_handle(),
        );
        l.tasks
            .push(l.handle.spawn(poll(src, shared, wake)).abort_handle());
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
    pub fn go(&mut self, raw: &str) {
        let Some(l) = &self.link else { return };
        match l.src.check(raw) {
            Err(e) => self.refusal = Some(refusal(e, raw)),
            Ok(url) => {
                self.refusal = None;
                self.act(Act::Go(url));
            }
        }
    }

    pub fn act(&mut self, a: Act) {
        let Some(l) = &self.link else { return };
        let (src, shared, mut wake) = (l.src.clone(), self.shared.clone(), self.wake.clone());
        l.handle.spawn(async move {
            let retarget = matches!(a, Act::Tab(_));
            // the view never follows a tab by itself: after a switch it is pointed at the new one
            let r = match src.act(a).await {
                Ok(()) if retarget => src.view(true).await,
                r => r,
            };
            lock(&shared).note = r.err();
            if let Ok(p) = src.page().await {
                lock(&shared).page = Some(p);
            }
            let _ = wake.try_send(());
        });
    }
}

/// `view(true)`, then the frame pump.
async fn start(src: Arc<dyn Source>, shared: Arc<Mutex<Shared>>, mut wake: Sender<()>) {
    if let Err(e) = src.view(true).await {
        lock(&shared).note = Some(e);
        let _ = wake.try_send(());
        return;
    }
    lock(&shared).note = None;
    let Some(mut feed) = src.frames() else { return };
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
/// a tab closed) re-points the view, since it never follows a tab by itself.
async fn poll(src: Arc<dyn Source>, shared: Arc<Mutex<Shared>>, mut wake: Sender<()>) {
    let mut last: Option<(usize, usize)> = None;
    loop {
        if let Ok(p) = src.page().await {
            let sig = (p.active, p.tabs.len());
            if last.is_some_and(|l| l != sig)
                && let Err(e) = src.view(true).await
            {
                lock(&shared).note = Some(e);
            }
            last = Some(sig);
            lock(&shared).page = Some(p);
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
            self.shown && self.active && self.ts.tab == ChatTab::Browser && !self.previewing();
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
            cx.subscribe_in(&bar, window, |this, bar, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    let raw = bar.read(cx).value().to_string();
                    this.ts.bv.go(&raw);
                    cx.notify();
                }
            })
            .detach();
            bp.bar = Some(bar);
        }
        // the page's address in the bar, unless the person is typing in it
        if let (Some(bar), Some(p)) = (bp.bar.clone(), page)
            && p.url != bp.bar_url
            && !bar.read(cx).focus_handle(cx).is_focused(window)
        {
            bp.bar_url = p.url.clone();
            bar.update(cx, |s, cx| s.set_value(p.url, window, cx));
        }
    }

    /// The URL bar holds the keys: the window-wide key hook leaves them alone.
    pub(super) fn bar_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.ts.tab == ChatTab::Browser
            && self
                .ts
                .bv
                .bar
                .as_ref()
                .is_some_and(|b| b.read(cx).focus_handle(cx).is_focused(window))
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
        bar = bar.child(
            div()
                .id("bv-view-only")
                .debug_selector(|| "browser-view-only".to_string())
                .child(panes::badge("view only".into(), dim())),
        );
        let mut col = div().flex().flex_col().gap(px(8.)).size_full().child(bar);
        if let Some(p) = page.as_ref().filter(|p| p.tabs.len() > 1) {
            col = col.child(div().flex().gap(px(4.)).overflow_hidden().children(
                p.tabs.iter().enumerate().map(|(i, title)| {
                    let on = i == p.active;
                    div()
                        .id(("bv-tab", i))
                        .px(px(10.))
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .max_w(px(180.))
                        .truncate()
                        .rounded(px(5.))
                        .text_size(px(12.))
                        .cursor_pointer()
                        .text_color(if on { strong() } else { dim() })
                        .when(on, |d| d.bg(hsla(t::ELEVATED)))
                        .when(!on, |d| {
                            d.hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.05)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.ts.bv.act(Act::Tab(i));
                            cx.notify();
                        }))
                        .child(title.clone())
                }),
            ));
        }
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
        } else if let Some(n) = note {
            col = col.child(
                div()
                    .text_size(px(12.5))
                    .text_color(hsla(t::WARN))
                    .child(sanitize_for_display(&n)),
            );
        }
        let pointer = self
            .live
            .as_ref()
            .and_then(|l| l.live.view.panes.pointer.clone())
            .filter(|p| p.ts.is_some_and(|ts| panes::pointer_alpha(now - ts) > 0.0));
        let stage = div()
            .debug_selector(|| "browser-stage".to_string())
            .relative()
            .flex_1()
            .min_h(px(120.))
            .w_full()
            .rounded(px(6.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .bg(hsla(t::BG))
            .overflow_hidden();
        col = col.child(match frame {
            Some(d) => {
                let frame_d = d.clone();
                stage.child(
                    canvas(
                        move |b, window, cx| {
                            prepare_page(b, &frame_d, pointer.as_ref(), now, motion, window, cx)
                        },
                        move |_, prepared, window, cx| {
                            if let Some((at, cursor)) = prepared {
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
                    .size_full(),
                )
            }
            None => stage
                .flex()
                .items_center()
                .justify_center()
                .text_color(dim())
                .child(frame_err.map_or_else(
                    || "Waiting for the first frame...".to_string(),
                    |e| format!("Frame not shown: {e}"),
                )),
        });
        col = col.child(
            div().text_size(px(12.)).text_color(dim()).child(
                "View only: clicks and keys are not sent to the page yet. The agent works in this browser and can read the pages open in it.",
            ),
        );
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

/// Where the frame goes inside `b` (letterboxed), with the agent cursor laid out over it (layout is only allowed
/// while preparing, so the cursor is built here and painted later).
fn prepare_page(
    b: Bounds<gpui::Pixels>,
    d: &Decoded,
    pointer: Option<&synthesis_view::agent::Pointer>,
    now: i64,
    motion: bool,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Option<(Bounds<gpui::Pixels>, Option<AnyElement>)> {
    let (x, y, w, h) = letterbox(
        f32::from(b.size.width),
        f32::from(b.size.height),
        d.w as f32,
        d.h as f32,
    )?;
    let at = Bounds::new(
        point(b.origin.x + px(x), b.origin.y + px(y)),
        size(px(w), px(h)),
    );
    let cursor = pointer
        .zip(pointer.and_then(|p| panes::cursor_on(p, d.w, d.h)))
        .map(|(p, (fx, fy))| {
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
}

#[cfg(any(test, feature = "shot"))]
impl FakeSource {
    pub fn new(jpeg: Vec<u8>, page: PageState) -> Arc<Self> {
        Arc::new(Self {
            jpeg,
            page: Mutex::new(page),
            calls: Mutex::default(),
            policy: UrlPolicy::default(),
        })
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
    fn view(&self, on: bool) -> BoxFuture<'static, Result<(), String>> {
        self.log(format!("view({on})"));
        Box::pin(async { Ok(()) })
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
        if let Act::Tab(i) = a {
            self.page.lock().unwrap().active = i;
        }
        Box::pin(async { Ok(()) })
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
    fn the_frame_is_scaled_to_fit_and_centred() {
        // wider pane: bars left and right
        assert_eq!(
            letterbox(1000., 400., 1280., 800.),
            Some((180., 0., 640., 400.))
        );
        // taller pane: bars above and below
        assert_eq!(
            letterbox(640., 800., 1280., 800.),
            Some((0., 200., 640., 400.))
        );
        // exact fit and a frame smaller than the pane (scaled up)
        assert_eq!(
            letterbox(1280., 800., 1280., 800.),
            Some((0., 0., 1280., 800.))
        );
        assert_eq!(
            letterbox(200., 200., 100., 50.),
            Some((0., 50., 200., 100.))
        );
        // no size, no frame
        assert_eq!(letterbox(0., 400., 1280., 800.), None);
        assert_eq!(letterbox(400., 400., 0., 800.), None);
        assert_eq!(letterbox(f32::NAN, 400., 10., 10.), None);
    }

    #[test]
    fn the_url_bar_refuses_what_the_browser_policy_refuses() {
        let p = UrlPolicy::default();
        for u in [
            "file:///etc/passwd",
            "http://169.254.169.254",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1",
            "http://localhost:3000",
            "localhost:3000",
            "http://[::1]/",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "ftp://example.com/",
            "  ",
        ] {
            assert!(check_bar(&p, u).is_err(), "{u} must be refused");
        }
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
            active: 0,
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
        bp.act(Act::Tab(1));
        wait("the re-target", || fake.calls().len() == 3);
        assert_eq!(fake.calls(), ["view(true)", "Tab(1)", "view(true)"]);
        // hidden (another tab, another session): view(false) and no pixels held
        bp.sync(false);
        wait("view(false)", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
        });
        assert!(bp.snapshot().0.is_none());
        // dropping a shown pane turns the view off too
        bp.sync(true);
        wait("view(true) again", || {
            fake.calls().iter().filter(|c| *c == "view(true)").count() == 3
        });
        drop(bp);
        wait("view(false) on drop", || {
            fake.calls().last().is_some_and(|c| c == "view(false)")
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
}
