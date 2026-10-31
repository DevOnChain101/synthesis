//! The window: title bar, sidebar and workspace skeleton after `design/03-designs/desktop/desktop.html`,
//! the onboarding screen, Settings (mode selector, Appearance font picker) and the Mode 1 terminal
//! pane. Logic lives in `onboarding`, `settings`, `credentials` and `vt`; this file only draws it.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use gpui::AnimationExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px,
};
use gpui_component::input::{Input, InputState};
use gpui_component::{ActiveTheme as _, Root, StyledExt as _, h_flex, v_flex};
use synthesis_config::Env;

use crate::activity;
use crate::convo::Convo;
use crate::credentials::CredentialStore;
use crate::engine::{self, Choice, Engine, UiMsg};
use crate::onboarding::{self, Kind, Outcome};
use crate::palette::{self, Command};
use crate::project::{self, Project};
use crate::pty::PtyShell;
use crate::settings::{self, DesktopSettings, FontChoice, Mode};
use crate::theme::{self, hsla};
use crate::vt;

mod views;

gpui::actions!(
    synthesis,
    [
        NewTask,
        OpenPalette,
        OpenConfiguration,
        AllowOnce,
        DenyPending,
        ToggleSidebar
    ]
);

/// Cmd (macOS) / Ctrl (elsewhere) shortcuts drawn in the design.
pub fn bindings() -> Vec<gpui::KeyBinding> {
    #[cfg(target_os = "macos")]
    let keys = [
        "cmd-k",
        "cmd-n",
        "cmd-,",
        "cmd-enter",
        "cmd-backspace",
        "cmd-b",
    ];
    #[cfg(not(target_os = "macos"))]
    let keys = [
        "ctrl-k",
        "ctrl-n",
        "ctrl-,",
        "ctrl-enter",
        "ctrl-backspace",
        "ctrl-b",
    ];
    vec![
        gpui::KeyBinding::new(keys[0], OpenPalette, None),
        gpui::KeyBinding::new(keys[1], NewTask, None),
        gpui::KeyBinding::new(keys[2], OpenConfiguration, None),
        gpui::KeyBinding::new(keys[3], AllowOnce, None),
        gpui::KeyBinding::new(keys[4], DenyPending, None),
        gpui::KeyBinding::new(keys[5], ToggleSidebar, None),
    ]
}

const FONT_SIZE: f32 = 13.0;

/// Left padding of the rail's top bar: room for the macOS traffic lights, then the brand mark.
pub(super) const LIGHTS_W: f32 = if cfg!(target_os = "macos") { 78. } else { 16. };
/// Most session tabs shown in the top bar before the overflow menu.
pub(super) const MAX_TABS: usize = 4;
/// Width of the Workspace icon rail; the Sessions panel is `DesktopSettings::sessions_w` wide.
pub(super) const RAIL_W: f32 = 64.;
/// Drag payload of the Sessions panel's right edge.
pub(super) struct SessionsResize;
impl Render for SessionsResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}
/// The header row of every content column: session title, page drag region.
pub(super) const HEADER_H: f32 = 48.;
/// Reading width of the conversation and width of the Inspector slide-over.
pub(super) const READING_W: f32 = 760.;
pub(super) const INSPECTOR_W: f32 = 560.;

/// A part of the full-bleed top edge that moves the window (double click zooms). Only for areas
/// with no interactive content.
pub(super) fn drag(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .window_control_area(gpui::WindowControlArea::Drag)
        .on_mouse_down(MouseButton::Left, |ev, window, _| {
            // Windows moves the window itself from the Drag area
            if cfg!(target_os = "windows") {
                return;
            }
            if ev.click_count >= 2 {
                if cfg!(target_os = "macos") {
                    window.titlebar_double_click();
                } else {
                    window.zoom_window();
                }
            } else {
                window.start_window_move();
            }
        })
}

/// Minimise, maximise and close, top right, for client-decorated windows (Windows, Linux). None on
/// macOS (native traffic lights) and under server-side decorations.
fn window_controls(window: &Window) -> Option<AnyElement> {
    if cfg!(target_os = "macos") {
        return None;
    }
    #[cfg(target_os = "linux")]
    if !matches!(
        window.window_decorations(),
        gpui::Decorations::Client { .. }
    ) {
        return None;
    }
    let linux = cfg!(target_os = "linux");
    let btn = |id: &'static str, ic: &'static str, area, hover: u32| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(40.))
            .h(px(32.))
            .rounded(px(8.))
            .window_control_area(area)
            .hover(move |d| d.bg(hsla(hover)))
            .child(icon(ic, 13., theme::MUTED))
    };
    use gpui::WindowControlArea as A;
    let max = if window.is_maximized() {
        "window-restore"
    } else {
        "window-maximize"
    };
    Some(
        h_flex()
            .absolute()
            .top(px((HEADER_H - 32.) / 2.))
            .right(px(8.))
            .gap(px(2.))
            .child(
                btn("wc-min", "window-minimize", A::Min, 0x1E242B)
                    .when(linux, |d| d.on_click(|_, w, _| w.minimize_window())),
            )
            .child(
                btn("wc-max", max, A::Max, 0x1E242B)
                    .when(linux, |d| d.on_click(|_, w, _| w.zoom_window())),
            )
            .child(
                btn("wc-close", "window-close", A::Close, theme::ERROR)
                    .when(linux, |d| d.on_click(|_, w, _| w.remove_window())),
            )
            .into_any_element(),
    )
}

/// Registers the bundled OFL fonts (apps/desktop/assets/fonts: the code fonts and Inter) with GPUI.
pub fn register_fonts(cx: &App) {
    let fonts: [&'static [u8]; 3] = [
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
        include_bytes!("../assets/fonts/FiraCode-Regular.ttf"),
        include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    ];
    if let Err(e) = cx.text_system().add_fonts(
        fonts
            .into_iter()
            .chain(crate::brand::UI_FONT_FILES)
            .map(Cow::Borrowed)
            .collect(),
    ) {
        eprintln!("synthesis-desktop: bundled fonts not loaded: {e}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Main,
}

/// Pages opened from the Workspace rail (Configuration) and from the row under the Sessions tabs
/// (docs/specs/desktop.md section 8, Navigation). `Session` is the home: conversation, file/diff
/// viewer and terminal panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Session,
    Inbox,
    Activity,
    Tools,
    Topology,
    Configuration,
}

/// Tabs of the second sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SideTab {
    Sessions,
    Explorer,
    Changes,
}

struct Form {
    kind: Kind,
    id: Entity<InputState>,
    url: Entity<InputState>,
    key: Entity<InputState>,
    model: Entity<InputState>,
    /// (is error, text): fixed messages only, never the typed values.
    status: Option<(bool, String)>,
    busy: bool,
}

/// One open session in the tab strip. The live one is fed by the engine; saved ones are replays
/// of the stored event log (read-only: resuming into the window is not wired yet).
struct Tab {
    id: String,
    title: String,
    convo: Convo,
    events: Vec<synthesis_events::Envelope>,
    live: bool,
}

impl Tab {
    fn new_live() -> Self {
        Self {
            id: String::new(),
            title: "New session".into(),
            convo: Convo::default(),
            events: vec![],
            live: true,
        }
    }
}

/// The file/diff viewer on the right.
struct Viewer {
    title: String,
    lines: Vec<String>,
    diff: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Model,
    Effort,
    Perm,
}

#[derive(Clone, PartialEq, Eq)]
enum EngineState {
    Idle,
    Starting,
    Ready,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bottom {
    Code,
    Terminal,
    Browser,
    Verification,
}

pub struct Shell {
    screen: Screen,
    nav: Nav,
    side_tab: SideTab,
    settings: DesktopSettings,
    home: PathBuf,
    /// The workspace (project directory) of the second sidebar and the engine.
    cwd: PathBuf,
    env: Env,
    store: Arc<dyn CredentialStore>,
    rt: Arc<tokio::runtime::Runtime>,
    form: Form,
    /// Provider whose key the engine reads from the store.
    provider: Option<String>,
    saved: Vec<synthesis_tui::SavedSession>,
    tabs: Vec<Tab>,
    active: usize,
    engine: Option<Arc<Engine>>,
    engine_state: EngineState,
    composer: Entity<InputState>,
    search: Entity<InputState>,
    model: Option<String>,
    effort: Option<String>,
    perm: synthesis_permissions::Mode,
    menu: Option<Menu>,
    proj: Project,
    branch: String,
    viewer: Option<Viewer>,
    convo_scroll: gpui::ScrollHandle,
    bottom: Bottom,
    bottom_term: Option<Entity<TermPane>>,
    filter: activity::Filter,
    /// Mode 1: the TUI with the real engine / the plain shell, created on first use.
    agent_term: Option<Entity<TermPane>>,
    shell_term: Option<Entity<TermPane>>,
    agent_on: bool,
    sheet: bool,
    /// Evidence cards the person collapsed (tool call ids); cards start open.
    collapsed: std::collections::HashSet<String>,
    /// Per changed file: lines added, removed and the head of its diff, taken when the tool ends.
    file_diffs: std::collections::HashMap<String, (u32, u32, Vec<String>)>,
    /// The Inspector slide-over; `bottom` is its tab.
    inspector_open: bool,
    /// The overflow menu of the top bar's session tabs.
    tabs_open: bool,
    /// A sidebar fold was toggled in this run: animate it (150 ms) instead of just applying it.
    side_anim: bool,
    /// The Sessions panel edge is being dragged; the width is saved on mouse up.
    sessions_resizing: bool,
    /// A running session whose tab is being closed, waiting for the confirmation (D-1).
    confirm_close: Option<usize>,
    focus: FocusHandle,
    /// The Cmd/Ctrl+K palette: open flag and its query input.
    palette_open: bool,
    palette_q: Entity<InputState>,
    clip: futures::channel::mpsc::UnboundedSender<String>,
    /// Development: scripted model and an automatic first prompt (feature `test-provider`).
    fake: Option<String>,
    auto_send: Option<String>,
}

fn input(
    window: &mut Window,
    cx: &mut Context<Shell>,
    placeholder: &str,
    value: &str,
    masked: bool,
) -> Entity<InputState> {
    let (p, v) = (placeholder.to_string(), value.to_string());
    cx.new(|cx| {
        let mut s = InputState::new(window, cx).placeholder(p).masked(masked);
        if !v.is_empty() {
            s = s.default_value(v);
        }
        s
    })
}

/// How the window starts (`main.rs`).
pub struct Launch {
    pub screen: Screen,
    pub nav: Nav,
    pub home: PathBuf,
    pub project: PathBuf,
    pub env: Env,
    pub store: Arc<dyn CredentialStore>,
    pub fake: Option<String>,
    pub auto_send: Option<String>,
    /// Development: write the window to this PNG after this many seconds, then quit.
    pub shot: Option<(PathBuf, u64)>,
    /// Development: start on this sidebar tab (`explorer`, `changes`) and open this file
    /// (`diff:PATH` opens its diff).
    pub side: Option<String>,
    pub open: Option<String>,
}

/// Where a terminal pane gets its bytes and where its keys go.
pub struct TermSource {
    bytes: futures::channel::mpsc::UnboundedReceiver<Vec<u8>>,
    input: Box<dyn Fn(Vec<u8>)>,
    resize: Box<dyn Fn(u32, u32)>,
    initial: (u32, u32),
    /// Dropped with the pane: ends the shell or the TUI thread.
    _hold: Box<dyn std::any::Any>,
}

impl Shell {
    pub fn new(l: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let Launch {
            screen,
            nav,
            home,
            project,
            env,
            store,
            fake,
            auto_send,
            shot: _,
            side,
            open: _,
        } = l;
        let remembered = settings::load_remembered(&home);
        let kind = match remembered.as_ref().map(|r| r.kind.as_str()) {
            Some("openai") => Kind::OpenAi,
            _ => Kind::Anthropic,
        };
        let (rid, rurl, rmodel) = remembered
            .as_ref()
            .map_or((kind.default_id(), kind.default_url(), ""), |r| {
                (r.id.as_str(), r.base_url.as_str(), r.model.as_str())
            });
        let form = Form {
            kind,
            id: input(window, cx, "provider id", rid, false),
            url: input(window, cx, "https://…", rurl, false),
            key: input(
                window,
                cx,
                "API key (kept in memory for this session)",
                "",
                true,
            ),
            model: input(
                window,
                cx,
                "model (optional, filled after the test)",
                rmodel,
                false,
            ),
            status: None,
            busy: false,
        };
        let provider = remembered.as_ref().map(|r| r.id.clone());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map(Arc::new)
            .expect("tokio runtime");
        let composer = input(
            window,
            cx,
            "Describe your goal, or paste a task…",
            "",
            false,
        );
        cx.subscribe_in(
            &composer,
            window,
            |this, _, ev: &gpui_component::input::InputEvent, window, cx| {
                if matches!(ev, gpui_component::input::InputEvent::PressEnter { .. }) {
                    this.send_composer(window, cx);
                }
            },
        )
        .detach();
        let palette_q = input(window, cx, "Search pages and actions…", "", false);
        cx.observe(&palette_q, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &palette_q,
            window,
            |this, state, ev: &gpui_component::input::InputEvent, window, cx| {
                if matches!(ev, gpui_component::input::InputEvent::PressEnter { .. }) {
                    let q = state.read(cx).value().to_string();
                    if let Some((_, c)) = palette::filter(&q).first() {
                        this.run_command(*c, window, cx);
                    }
                }
            },
        )
        .detach();
        let search = input(window, cx, "Search sessions…", "", false);
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        let (clip, mut clip_rx) = futures::channel::mpsc::unbounded::<String>();
        cx.spawn(async move |this, cx| {
            while let Some(t) = clip_rx.next().await {
                if this
                    .update(cx, |_, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(t));
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let cwd = synthesis_app::run::workspace_root(&project, &home);
        let saved = synthesis_tui::list_saved(&home, None).unwrap_or_default();
        let st = settings::load(&home);
        Self {
            screen,
            nav,
            side_tab: match side.as_deref() {
                Some("explorer") => SideTab::Explorer,
                Some("changes") => SideTab::Changes,
                _ => SideTab::Sessions,
            },
            settings: st,
            proj: project::scan(&cwd),
            branch: project::branch(&cwd),
            cwd,
            home,
            env,
            store,
            rt,
            form,
            provider,
            saved,
            tabs: vec![Tab::new_live()],
            active: 0,
            engine: None,
            engine_state: EngineState::Idle,
            composer,
            search,
            model: None,
            effort: None,
            perm: synthesis_permissions::Mode::Default,
            menu: None,
            viewer: None,
            convo_scroll: gpui::ScrollHandle::new(),
            bottom: Bottom::Terminal,
            bottom_term: None,
            filter: activity::Filter::All,
            agent_term: None,
            shell_term: None,
            agent_on: !st.plain_terminal,
            sheet: false,
            collapsed: Default::default(),
            file_diffs: Default::default(),
            inspector_open: false,
            tabs_open: false,
            side_anim: false,
            sessions_resizing: false,
            confirm_close: None,
            focus: cx.focus_handle(),
            palette_open: false,
            palette_q,
            clip,
            fake,
            auto_send,
        }
    }

    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.side_anim = true;
        if let Err(e) =
            settings::save_sidebar_collapsed(&self.home, self.settings.sidebar_collapsed)
        {
            eprintln!("synthesis-desktop: sidebar state not saved: {e}");
        }
        cx.notify();
    }

    /// Closes a session tab. A stored session just leaves the strip. The live session ends: when
    /// it is running or waiting for you, that first needs a confirmation (docs/specs/desktop.md
    /// D-1: closing cancels the task). Nothing is deleted: the history stays in the store.
    pub(super) fn close_tab(&mut self, i: usize, confirmed: bool, cx: &mut Context<Self>) {
        let Some(t) = self.tabs.get(i) else { return };
        if t.live {
            let busy = t.convo.running || t.convo.needs_you() > 0;
            if t.convo.items.is_empty() && !busy {
                return;
            }
            if busy && !confirmed {
                self.confirm_close = Some(i);
                cx.notify();
                return;
            }
            if let Some(e) = &self.engine {
                e.cancel();
            }
            self.new_session(cx);
        } else {
            self.tabs.remove(i);
            self.active = self.active.min(self.tabs.len() - 1);
        }
        self.tabs_open = false;
        self.confirm_close = None;
        cx.notify();
    }

    fn confirm_overlay(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("confirm-backdrop")
            .absolute()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(160.))
            .bg(hsla(0x000000).opacity(0.45))
            .on_click(cx.listener(|this, _, _, cx| {
                this.confirm_close = None;
                cx.notify();
            }))
            .child(
                v_flex()
                    .id("confirm")
                    .w(px(420.))
                    .h_auto()
                    .gap(px(16.))
                    .p(px(24.))
                    .rounded(px(14.))
                    .bg(hsla(theme::PANEL))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_semibold()
                            .child("Cancel the running task?"),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .line_height(px(20.))
                            .text_color(hsla(theme::MUTED))
                            .child(
                                "Closing this session cancels its task. The history stays in the Sessions list.",
                            ),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                btn("keep-open", None, "Keep open", Variant::Ghost, None).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.confirm_close = None;
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(
                                btn(
                                    "cancel-close",
                                    None,
                                    "Cancel task and close",
                                    Variant::Primary,
                                    None,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.close_tab(i, true, cx)
                                })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen != Screen::Main {
            return;
        }
        self.palette_open = true;
        self.palette_q
            .update(cx, |q, cx| q.set_value("", window, cx));
        window.focus(&self.palette_q.focus_handle(cx), cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn run_command(&mut self, c: Command, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        match c {
            Command::NewTask => self.new_session(cx),
            Command::Sessions => {
                self.nav = Nav::Session;
                self.side_tab = SideTab::Sessions;
            }
            Command::Inbox => self.nav = Nav::Inbox,
            Command::Activity => self.nav = Nav::Activity,
            Command::Tools => self.nav = Nav::Tools,
            Command::Topology => self.nav = Nav::Topology,
            Command::Configuration => self.nav = Nav::Configuration,
        }
        cx.notify();
    }

    fn palette_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let q = self.palette_q.read(cx).value().to_string();
        let rows = palette::filter(&q)
            .into_iter()
            .enumerate()
            .map(|(i, (text, c))| {
                div()
                    .id(SharedString::from(format!("pal-{i}")))
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .text_size(px(13.))
                    .cursor_pointer()
                    .when(i == 0, |d| d.bg(hsla(theme::NAV_ACTIVE)))
                    .child(text)
                    .on_click(cx.listener(move |this, _, w, cx| this.run_command(c, w, cx)))
            });
        div()
            .id("palette-backdrop")
            .absolute()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(110.))
            .bg(hsla(0x000000).opacity(0.45))
            .on_click(cx.listener(|this, _, w, cx| this.close_palette(w, cx)))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                if ev.keystroke.key == "escape" {
                    this.close_palette(w, cx);
                }
            }))
            .child(
                v_flex()
                    .id("palette")
                    .w(px(480.))
                    .h_auto()
                    .max_h(px(380.))
                    .gap(px(6.))
                    .p(px(8.))
                    .rounded(px(14.))
                    .border_1()
                    .border_color(hsla(theme::INPUT_BORDER))
                    .bg(hsla(theme::PANEL))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(Input::new(&self.palette_q).appearance(false))
                    .child(div().h(px(1.)).bg(hsla(theme::LINE)))
                    .children(rows),
            )
            .into_any_element()
    }

    fn project_name(&self) -> String {
        self.cwd.file_name().map_or_else(
            || self.cwd.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    /// Called when the main screen is shown: starts the engine and the terminal panel.
    pub fn enter_main(&mut self, cx: &mut Context<Self>) {
        if self.settings.mode == Mode::Gpui {
            self.ensure_bottom_term(cx);
            self.start_engine(cx);
        } else if self.settings.plain_terminal {
            self.start_engine(cx);
        }
    }

    fn make_term(&self, source: TermSource, cx: &mut Context<Self>) -> Entity<TermPane> {
        cx.new(|cx| TermPane::new(source, cx))
    }

    fn ensure_bottom_term(&mut self, cx: &mut Context<Self>) {
        if self.bottom_term.is_none() {
            self.bottom_term = self.shell_source(100, 10).map(|s| self.make_term(s, cx));
        }
    }

    /// A real shell in the workspace folder (Unix; see `pty`).
    fn shell_source(&self, cols: u32, rows: u32) -> Option<TermSource> {
        match PtyShell::spawn(
            self.env.get_nonempty("SHELL").unwrap_or("/bin/sh"),
            &self.cwd,
            cols as u16,
            rows as u16,
        ) {
            Ok((sh, bytes)) => {
                let sh = Arc::new(Mutex::new(sh));
                let (w, r) = (sh.clone(), sh.clone());
                Some(TermSource {
                    bytes,
                    input: Box::new(move |b| {
                        w.lock().unwrap_or_else(|e| e.into_inner()).write(&b);
                    }),
                    resize: Box::new(move |c, ro| {
                        r.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .resize(c as u16, ro as u16);
                    }),
                    initial: (cols, rows),
                    _hold: Box::new(sh),
                })
            }
            Err(e) => {
                eprintln!("synthesis-desktop: no shell panel: {e}");
                None
            }
        }
    }

    fn ensure_agent_term(&mut self, cx: &mut Context<Self>) {
        if self.agent_term.is_some() {
            return;
        }
        let mut rig = crate::agent_tui::start(
            100,
            30,
            self.cwd.clone(),
            self.engine_env(),
            self.clip.clone(),
            self.fake.clone(),
        );
        let bytes = std::mem::replace(&mut rig.bytes, futures::channel::mpsc::unbounded().1);
        let keys = rig.keys.clone();
        let size = rig.size.clone();
        let src = TermSource {
            bytes,
            input: Box::new(move |b| {
                let _ = keys.send(b);
            }),
            resize: Box::new(move |c, r| {
                *size.lock().unwrap_or_else(|e| e.into_inner()) = (c, r);
            }),
            initial: (100, 30),
            _hold: Box::new(rig),
        };
        self.agent_term = Some(self.make_term(src, cx));
    }

    fn ensure_shell_term(&mut self, cx: &mut Context<Self>) {
        if self.shell_term.is_none() {
            self.shell_term = self.shell_source(100, 30).map(|s| self.make_term(s, cx));
        }
    }

    /// `env` with the key from the store (memory only) for the engine's `prepare`.
    fn engine_env(&self) -> Env {
        match self
            .provider
            .as_deref()
            .and_then(|p| self.store.get(p).ok().flatten().map(|k| (p, k)))
        {
            Some((p, k)) => engine::env_with_key(&self.env, p, &k),
            None => self.env.clone(),
        }
    }

    fn close_engine(&mut self) {
        if let Some(a) = self.engine.take()
            && let Ok(e) = Arc::try_unwrap(a)
        {
            std::thread::spawn(move || e.close());
        }
    }

    fn start_engine(&mut self, cx: &mut Context<Self>) {
        if self.engine_state == EngineState::Starting {
            return;
        }
        self.close_engine();
        self.engine_state = EngineState::Starting;
        let (project, env, model, fake) = (
            self.cwd.clone(),
            self.engine_env(),
            self.model.clone(),
            self.fake.clone(),
        );
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(Engine::start(
                &project,
                &env,
                model.as_deref(),
                fake.as_deref(),
            ));
        });
        cx.spawn(async move |this, cx| {
            let r = rx.await;
            let _ = this.update(cx, |this, cx| match r {
                Ok(Ok((e, mut events))) => {
                    if this.model.is_none() {
                        this.model = Some(e.label.clone());
                    }
                    this.engine = Some(Arc::new(e));
                    this.engine_state = EngineState::Ready;
                    cx.spawn(async move |this, cx| {
                        while let Some(m) = events.next().await {
                            if this.update(cx, |this, cx| this.on_msg(m, cx)).is_err() {
                                break;
                            }
                        }
                    })
                    .detach();
                    if let Some(t) = this.auto_send.take() {
                        this.send_text(t, cx);
                    }
                    cx.notify();
                }
                Ok(Err(e)) => {
                    this.engine_state = EngineState::Failed(e);
                    cx.notify();
                }
                Err(_) => {
                    this.engine_state = EngineState::Failed("the engine thread ended".into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn live_tab(&mut self) -> &mut Tab {
        &mut self.tabs[0]
    }

    fn on_msg(&mut self, m: UiMsg, cx: &mut Context<Self>) {
        match m {
            UiMsg::Event(e) => {
                let t = self.live_tab();
                t.convo.apply(&e);
                if t.title == "New session"
                    && let synthesis_events::Event::UserMessage { text, .. } = &e.event
                {
                    t.title = text.lines().next().unwrap_or("").chars().take(64).collect();
                }
                t.events.push(*e);
                self.record_file_diff();
            }
            UiMsg::TaskDone(r) => {
                if let Err(e) = r {
                    self.live_tab()
                        .convo
                        .items
                        .push(crate::convo::Item::Notice {
                            warning: true,
                            text: e,
                        });
                }
                self.live_tab().convo.running = false;
                self.refresh_project();
            }
        }
        cx.notify();
    }

    /// Diff head and +N -M of the file a finished Write/Edit tool changed (for its evidence card).
    fn record_file_diff(&mut self) {
        let Some(synthesis_events::Event::ToolCompleted { tool_call_id, .. }) =
            self.tabs[0].events.last().map(|e| &e.event)
        else {
            return;
        };
        let file = self.tabs[0].convo.items.iter().find_map(|i| match i {
            crate::convo::Item::Tool { id, file, .. } if id == tool_call_id => file.clone(),
            _ => None,
        });
        if let Some(f) = file {
            let mut lines = project::diff_of(&self.cwd, &f);
            let (a, r) = project::count_changes(&lines);
            lines.truncate(40);
            self.file_diffs.insert(f, (a, r, lines));
        }
    }

    fn refresh_project(&mut self) {
        self.proj = project::scan(&self.cwd);
        self.branch = project::branch(&self.cwd);
        self.saved = synthesis_tui::list_saved(&self.home, None).unwrap_or_default();
    }

    fn send_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.composer
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.send_text(text, cx);
    }

    fn send_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.active = 0;
        let note = |this: &mut Self, t: &str| {
            this.live_tab()
                .convo
                .items
                .push(crate::convo::Item::Notice {
                    warning: true,
                    text: t.into(),
                });
        };
        match self.engine.clone() {
            Some(e) => {
                e.set_mode(self.perm);
                e.set_effort(self.effort.clone());
                if let Err(m) = e.send(text) {
                    note(self, m);
                }
            }
            None => note(
                self,
                "The engine is not running yet: finish Configuration first.",
            ),
        }
        self.sheet = true;
        cx.notify();
    }

    /// The keyboard answer (Cmd/Ctrl+Enter, Cmd/Ctrl+Backspace) to the oldest pending approval of
    /// the live session. Never the default of a plain Enter.
    fn answer_first(&mut self, c: Choice, cx: &mut Context<Self>) {
        let id = self.tabs[0].convo.items.iter().find_map(|i| match i {
            crate::convo::Item::Approval {
                id,
                state: crate::convo::ApprovalState::Pending,
                ..
            } => Some(id.clone()),
            _ => None,
        });
        if let Some(id) = id {
            self.answer(&id, c, cx);
        }
    }

    fn answer(&mut self, id: &str, c: Choice, cx: &mut Context<Self>) {
        if let Some(e) = &self.engine {
            e.answer(id, c);
        }
        cx.notify();
    }

    fn set_model(&mut self, m: String, cx: &mut Context<Self>) {
        if self.model.as_deref() != Some(&m) {
            self.model = Some(m);
            self.tabs[0] = Tab::new_live();
            self.start_engine(cx);
        }
        self.menu = None;
        cx.notify();
    }

    fn select_workspace(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        if cwd != self.cwd {
            self.cwd = cwd;
            self.tabs = vec![Tab::new_live()];
            self.active = 0;
            self.viewer = None;
            self.bottom_term = None;
            self.refresh_project();
            self.ensure_bottom_term(cx);
            self.start_engine(cx);
        }
        self.nav = Nav::Session;
        cx.notify();
    }

    fn open_saved(&mut self, id: &str, cx: &mut Context<Self>) {
        self.nav = Nav::Session;
        if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
            self.active = i;
        } else {
            match synthesis_tui::load_resume(&self.home, id) {
                Ok(r) => {
                    let title = self
                        .saved
                        .iter()
                        .find(|s| s.id == id)
                        .map_or_else(|| id.to_string(), |s| s.title.chars().take(64).collect());
                    self.tabs.push(Tab {
                        id: id.into(),
                        title,
                        convo: Convo::from_events(&r.events),
                        events: r.events,
                        live: false,
                    });
                    self.active = self.tabs.len() - 1;
                }
                Err(e) => eprintln!("synthesis-desktop: cannot open session: {e}"),
            }
        }
        cx.notify();
    }

    /// Development (`--open session`): the newest stored session of this workspace as a second tab,
    /// the live one staying in front.
    fn open_latest_saved_in_background(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.display().to_string();
        let open: Vec<&str> = self.tabs.iter().map(|t| t.id.as_str()).collect();
        let next = self
            .saved
            .iter()
            .find(|s| s.cwd == cwd && !open.contains(&s.id.as_str()))
            .map(|s| s.id.clone());
        if let Some(id) = next {
            self.open_saved(&id, cx);
            self.active = 0;
        }
    }

    fn open_file(&mut self, path: &str, diff: bool, cx: &mut Context<Self>) {
        let lines = if diff {
            project::diff_of(&self.cwd, path)
        } else {
            project::read_head(&self.cwd, path)
        };
        self.viewer = Some(Viewer {
            title: path.rsplit('/').next().unwrap_or(path).to_string(),
            lines,
            diff,
        });
        self.nav = Nav::Session;
        self.bottom = Bottom::Code;
        self.inspector_open = true;
        cx.notify();
    }

    fn pending_total(&self) -> usize {
        self.tabs.iter().map(|t| t.convo.needs_you()).sum()
    }

    fn font(&self) -> SharedString {
        SharedString::from(self.settings.font.family())
    }

    fn set_font(&mut self, f: FontChoice, cx: &mut Context<Self>) {
        self.settings.font = f;
        if let Err(e) = settings::save_font(&self.home, f) {
            eprintln!("synthesis-desktop: font not saved: {e}");
        }
        theme::apply(f.family(), cx);
        cx.notify();
    }

    fn set_mode(&mut self, mode: Mode, plain: bool, cx: &mut Context<Self>) {
        self.settings.mode = mode;
        self.settings.plain_terminal = plain;
        for (k, v) in [("mode", mode.key().into()), ("plainTerminal", plain.into())] {
            if let Err(e) = settings::set(&self.home, k, v) {
                eprintln!("synthesis-desktop: {k} not saved: {e}");
            }
        }
        cx.notify();
    }

    fn pick_kind(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.form.kind;
        self.form.kind = kind;
        // only replace values the user has not edited
        for (state, was, now) in [
            (&self.form.id, old.default_id(), kind.default_id()),
            (&self.form.url, old.default_url(), kind.default_url()),
        ] {
            if state.read(cx).value().as_ref() == was {
                state.update(cx, |s, cx| s.set_value(now, window, cx));
            }
        }
        cx.notify();
    }

    fn typed(&self, cx: &App) -> Result<onboarding::Valid, onboarding::Problem> {
        let f = &self.form;
        onboarding::validate(
            f.kind,
            &f.id.read(cx).value(),
            &f.url.read(cx).value(),
            &f.key.read(cx).value(),
        )
    }

    fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let valid = match self.typed(cx) {
            Ok(v) => v,
            Err(p) => {
                self.form.status = Some((true, p.text().into()));
                return cx.notify();
            }
        };
        self.form.busy = true;
        self.form.status = Some((false, "Testing…".into()));
        cx.notify();
        let (tx, rx) = futures::channel::oneshot::channel();
        let env = self.env.clone();
        self.rt.spawn(async move {
            let _ = tx.send(onboarding::test_connection(&env, &valid).await);
        });
        cx.spawn_in(window, async move |this, cx| {
            let outcome = rx.await.unwrap_or(Outcome::Unreachable);
            let _ = this.update_in(cx, |this, window, cx| {
                this.form.busy = false;
                this.form.status = Some((!matches!(outcome, Outcome::Ok { .. }), outcome.text()));
                if let Outcome::Ok { models } = &outcome
                    && let Some(m) = models.first()
                    && this.form.model.read(cx).value().is_empty()
                {
                    let m = m.clone();
                    this.form
                        .model
                        .update(cx, |s, cx| s.set_value(m, window, cx));
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let valid = match self.typed(cx) {
            Ok(v) => v,
            Err(p) => {
                self.form.status = Some((true, p.text().into()));
                return cx.notify();
            }
        };
        let model = self.form.model.read(cx).value().trim().to_string();
        let model = (!model.is_empty()).then_some(model);
        if let Err(e) = onboarding::write_config(
            &self.home,
            &valid.id,
            valid.kind,
            &valid.url.url,
            model.as_deref(),
        ) {
            // the io error names files and schema paths, never a value we wrote
            self.form.status = Some((true, format!("Could not write synthesis.json: {e}")));
            return cx.notify();
        }
        if let Err(e) = self.store.set(&valid.id, valid.key.clone()) {
            self.form.status = Some((true, e.to_string()));
            return cx.notify();
        }
        self.form
            .key
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.form.status = None;
        let remembered = settings::Remembered {
            kind: valid.kind.protocol().as_str().into(),
            id: valid.id.clone(),
            base_url: valid.url.url.to_string(),
            model: model.clone().unwrap_or_default(),
        };
        if let Err(e) = settings::save_remembered(&self.home, &remembered) {
            eprintln!("synthesis-desktop: provider not remembered: {e}");
        }
        self.provider = Some(valid.id);
        self.model = None;
        self.screen = Screen::Main;
        self.enter_main(cx);
        cx.notify();
    }
}

// ---------------------------------------------------------------------------------- drawing

/// Type scale (Inter): 11 caps labels with tracking (medium, muted), 12 small, 13 body, 15 titles
/// (semibold), 22 greeting (medium). Labels are tracked caps in the muted colour.
fn label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(11.))
        .font_medium()
        .text_color(hsla(theme::MUTED))
        .child(
            text.into()
                .to_uppercase()
                .chars()
                .map(String::from)
                .collect::<Vec<_>>()
                .join("\u{200A}"),
        )
}

/// The three button variants of the app. Height 32, radius 10, hover and pressed states.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Variant {
    /// One per view: the action to take (pearl fill).
    Primary,
    Secondary,
    /// Quiet, text only until hovered.
    Ghost,
}

pub(super) fn btn(
    id: impl Into<gpui::ElementId>,
    lead: Option<&str>,
    text: &str,
    v: Variant,
    hint: Option<String>,
) -> gpui::Stateful<gpui::Div> {
    let (bg, fg, hover, press) = match v {
        Variant::Primary => (0xEBE9E6, 0x151619, 0xF7F5F2, 0xD6D2CD),
        Variant::Secondary => (0x20252C, theme::TEXT, 0x282E37, 0x1A1F26),
        Variant::Ghost => (0x0B0D10, theme::SILVER, 0x1A1F26, 0x14181D),
    };
    h_flex()
        .id(id)
        .h(px(32.))
        .px(px(14.))
        .gap(px(8.))
        .rounded(px(10.))
        .text_size(px(13.))
        .font_medium()
        .text_color(hsla(fg))
        .cursor_pointer()
        .when(v == Variant::Ghost, |d| d.bg(hsla(bg).opacity(0.)))
        .when(v != Variant::Ghost, |d| d.bg(hsla(bg)))
        .hover(move |d| d.bg(hsla(hover)))
        .active(move |d| d.bg(hsla(press)))
        .children(lead.map(|l| icon(l, 14., fg)))
        .child(text.to_string())
        .children(hint.map(|h| {
            div()
                .text_size(px(11.))
                .font_normal()
                .text_color(hsla(fg).opacity(0.55))
                .child(h)
        }))
}

/// A selectable option: the primary look when selected, secondary otherwise.
fn choice(id: impl Into<gpui::ElementId>, text: &str, selected: bool) -> gpui::Stateful<gpui::Div> {
    btn(
        id,
        None,
        text,
        if selected {
            Variant::Primary
        } else {
            Variant::Secondary
        },
        None,
    )
    .h(px(30.))
}

/// A Lucide icon from the bundled set (one set, one stroke weight), `size` px.
pub(super) fn icon(name: &str, size: f32, rgb: u32) -> gpui_component::Icon {
    use gpui::Styled as _;
    use gpui_component::{Icon, Sizable as _};
    Icon::default()
        .path(format!("icons/{name}.svg"))
        .with_size(px(size))
        .text_color(hsla(rgb))
}

/// Letter-spaced caps for the wordmark (GPUI has no letter-spacing style).
pub(super) fn spaced(text: &str) -> String {
    text.chars()
        .map(String::from)
        .collect::<Vec<_>>()
        .join("\u{200A}\u{2009}")
}

/// The shortcut text drawn next to an action: Cmd on macOS, Ctrl elsewhere.
pub(super) fn kbd(key: &str) -> String {
    format!(
        "{} {key}",
        if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        }
    )
}

fn empty(text: &str) -> gpui::Div {
    div()
        .p(px(16.))
        .text_size(px(12.))
        .text_color(hsla(theme::MUTED))
        .child(text.to_string())
}

impl Shell {
    fn onboarding(&self, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.form;
        let field = |name: &str, state: &Entity<InputState>| {
            v_flex()
                .gap(px(6.))
                .child(label(name.to_string()))
                .child(Input::new(state))
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .w(px(520.))
                    .gap(px(16.))
                    .p(px(24.))
                    .border_1()
                    .border_color(hsla(theme::LINE))
                    .rounded(px(16.))
                    .bg(hsla(theme::PANEL))
                    .child(
                        h_flex()
                            .gap(px(10.))
                            .child(gpui::img(crate::brand::mark()).size(px(34.)))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_semibold()
                                    .child(spaced("SYNTHESIS")),
                            ),
                    )
                    .child(div().text_size(px(20.)).child("Connect a model provider"))
                    .child(div().text_size(px(12.)).text_color(hsla(theme::MUTED)).child(
                        "Your key stays in memory for this session. It is never written to disk, logs or events.",
                    ))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(choice("k-anthropic", Kind::Anthropic.label(), f.kind == Kind::Anthropic).on_click(
                                cx.listener(|this, _, w, cx| this.pick_kind(Kind::Anthropic, w, cx)),
                            ))
                            .child(choice("k-openai", Kind::OpenAi.label(), f.kind == Kind::OpenAi).on_click(
                                cx.listener(|this, _, w, cx| this.pick_kind(Kind::OpenAi, w, cx)),
                            )),
                    )
                    .child(field("Provider id", &f.id))
                    .child(field("Base URL", &f.url))
                    .child(field("API key", &f.key))
                    .child(field("Model", &f.model))
                    .children(f.status.as_ref().map(|(err, t)| {
                        div()
                            .text_size(px(12.))
                            .text_color(hsla(if *err { theme::ERROR } else { theme::SUCCESS }))
                            .child(t.clone())
                    }))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .justify_end()
                            .child(
                                btn(
                                    "test",
                                    None,
                                    if f.busy { "Testing…" } else { "Test connection" },
                                    Variant::Secondary,
                                    None,
                                )
                                .on_click(cx.listener(|this, _, w, cx| this.test_connection(w, cx))),
                            )
                            .child(
                                btn("save", None, "Save and continue", Variant::Primary, None)
                                    .on_click(cx.listener(|this, _, w, cx| this.save(w, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.settings;
        let section = |t: &str| {
            div()
                .mt(px(8.))
                .text_size(px(13.))
                .font_semibold()
                .child(t.to_string())
        };
        let fonts = FontChoice::ALL.into_iter().map(|f| {
            choice(f.key(), f.label(), s.font == f)
                .on_click(cx.listener(move |this, _, _, cx| this.set_font(f, cx)))
        });
        let modes = [
            ("m-tui", "TUI", Mode::Tui, false),
            ("m-plain", "TUI · plain terminal", Mode::Tui, true),
            ("m-gpui", "GPUI", Mode::Gpui, false),
        ]
        .into_iter()
        .map(|(id, text, mode, plain)| {
            choice(id, text, s.mode == mode && s.plain_terminal == plain)
                .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, plain, cx)))
        });
        v_flex()
            .size_full()
            .px(px(24.))
            .pb(px(24.))
            .gap(px(12.))
            .child(div().text_size(px(15.)).font_semibold().child("Configuration"))
            .child(section("Interface"))
            .child(div().text_size(px(12.)).text_color(hsla(theme::MUTED)).child(
                "TUI draws the terminal interface in this window; GPUI is the native interface.",
            ))
            .child(h_flex().gap(px(8.)).children(modes))
            .child(section("Appearance"))
            .child(div().text_size(px(12.)).text_color(hsla(theme::MUTED)).child(
                "Font for code, diff and terminal. The interface itself uses Inter. Fonts are bundled (SIL OFL).",
            ))
            .child(h_flex().gap(px(8.)).children(fonts))
            .into_any_element()
    }
}

trait ActiveNav {
    fn when_active(self, active: bool) -> Self;
}

impl ActiveNav for gpui::Stateful<gpui::Div> {
    fn when_active(self, active: bool) -> Self {
        if active {
            self.bg(hsla(theme::NAV_ACTIVE))
        } else {
            self
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.screen {
            Screen::Onboarding => v_flex()
                .size_full()
                .child(drag("drag-onb").h(px(HEADER_H)).w_full())
                .child(div().flex_1().min_h_0().child(self.onboarding(cx)))
                .into_any_element(),
            Screen::Main => {
                let own_strip = self.nav == Nav::Session && self.settings.mode == Mode::Gpui;
                let content = match self.nav {
                    Nav::Configuration => self.settings_page(cx),
                    Nav::Activity => self.activity_page(cx),
                    Nav::Tools => self.tools_page(),
                    Nav::Topology => self.topology_page(),
                    Nav::Inbox => self.inbox_page(cx),
                    Nav::Session => match self.settings.mode {
                        Mode::Gpui => self.session_view(cx),
                        Mode::Tui => self.mode1(cx),
                    },
                };
                let content = if own_strip {
                    content
                } else {
                    v_flex()
                        .size_full()
                        .child(drag("drag-page").h(px(HEADER_H)).w_full())
                        .child(div().flex_1().min_h_0().child(content))
                        .into_any_element()
                };
                let collapsed = self.settings.sidebar_collapsed;
                let side_w = RAIL_W + self.settings.sessions_w();
                let side = h_flex()
                    .h_full()
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(self.workspace_rail(cx))
                    .child(self.sessions_panel(cx));
                let side = if self.side_anim {
                    side.with_animation(
                        SharedString::from(format!("side-{collapsed}")),
                        gpui::Animation::new(std::time::Duration::from_millis(150))
                            .with_easing(gpui::ease_out_quint()),
                        move |d, t| d.w(px(side_w * if collapsed { 1. - t } else { t })),
                    )
                    .into_any_element()
                } else if collapsed {
                    side.w(px(0.)).into_any_element()
                } else {
                    side.into_any_element()
                };
                h_flex()
                    .size_full()
                    .relative()
                    // landing: a soft warm wash over the whole window with the swoosh as faint
                    // art bleeding off the right edge; the sidebars are translucent over it
                    .when(self.nav == Nav::Session && self.is_landing(), |d| {
                        d.bg(gpui::linear_gradient(
                            200.,
                            gpui::linear_color_stop(hsla(0x1F1E1D), 0.),
                            gpui::linear_color_stop(hsla(theme::BG), 0.6),
                        ))
                        .child(
                            gpui::img(crate::brand::mark())
                                .absolute()
                                .top(px(-90.))
                                .right(px(-110.))
                                .size(px(460.))
                                .opacity(0.12),
                        )
                    })
                    .on_drag_move(cx.listener(
                        |this, e: &gpui::DragMoveEvent<SessionsResize>, _, cx| {
                            let (_, min, max) = settings::SESSIONS_W;
                            this.settings.sessions_width =
                                (f32::from(e.event.position.x) - RAIL_W).clamp(min, max) as u32;
                            this.sessions_resizing = true;
                            cx.notify();
                        },
                    ))
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            if std::mem::take(&mut this.sessions_resizing)
                                && let Err(e) = settings::save_sessions_width(
                                    &this.home,
                                    this.settings.sessions_w(),
                                )
                            {
                                eprintln!("synthesis-desktop: sidebar width not saved: {e}");
                            }
                        }),
                    )
                    .child(side)
                    .child(div().flex_1().min_w_0().h_full().child(content))
                    .into_any_element()
            }
        };
        let palette = self.palette_open.then(|| self.palette_overlay(cx));
        let confirm = self.confirm_close.map(|i| self.confirm_overlay(i, cx));
        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .key_context("Shell")
            .on_action(cx.listener(|this, _: &OpenPalette, w, cx| this.open_palette(w, cx)))
            .on_action(cx.listener(|this, _: &NewTask, _, cx| {
                if this.screen == Screen::Main {
                    this.new_session(cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &AllowOnce, _, cx| this.answer_first(Choice::Once, cx)),
            )
            .on_action(
                cx.listener(|this, _: &DenyPending, _, cx| this.answer_first(Choice::Deny, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                if this.screen == Screen::Main {
                    this.toggle_sidebar(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenConfiguration, _, cx| {
                if this.screen == Screen::Main {
                    this.nav = Nav::Configuration;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" && this.inspector_open {
                    this.inspector_open = false;
                    cx.notify();
                }
            }))
            .bg(hsla(theme::BG))
            .text_color(hsla(theme::TEXT))
            .font_family(crate::brand::UI_FONT)
            .child(div().flex_1().min_h_0().child(body))
            .children(palette)
            .children(confirm)
            .children(window_controls(window))
    }
}

// ------------------------------------------------------------------------- terminal pane

/// A terminal grid fed by a [`TermSource`]: the TUI (agent mode), a shell on a PTY, or both in
/// turn. The pane owns the source for its lifetime.
pub struct TermPane {
    grid: vt::Grid,
    source: TermSource,
    cells: Arc<Mutex<(u32, u32)>>,
    sent: (u32, u32),
    focus: FocusHandle,
    /// Take keyboard focus on first paint (the main terminal), not for side panels.
    grab_focus: bool,
}

impl TermPane {
    fn new(mut source: TermSource, cx: &mut Context<Self>) -> Self {
        let (c, r) = source.initial;
        let bytes = std::mem::replace(&mut source.bytes, futures::channel::mpsc::unbounded().1);
        let pane = Self {
            grid: vt::Grid::new(c as usize, r as usize),
            source,
            cells: Arc::new(Mutex::new((c, r))),
            sent: (c, r),
            focus: cx.focus_handle(),
            grab_focus: false,
        };
        let mut bytes = bytes;
        cx.spawn(async move |this, cx| {
            while let Some(b) = bytes.next().await {
                if this
                    .update(cx, |this, cx| {
                        this.grid.feed(&b);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        pane
    }
}

impl Focusable for TermPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TermPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (c, r) = *self.cells.lock().unwrap_or_else(|e| e.into_inner());
        self.grid.resize(c as usize, r as usize);
        if (c, r) != self.sent {
            self.sent = (c, r);
            (self.source.resize)(c, r);
        }
        let rows: Vec<_> = (0..self.grid.size().1).map(|i| self.grid.runs(i)).collect();
        let cursor = self.grid.cursor();
        let family = cx.theme().mono_font_family.clone();
        let cells = self.cells.clone();
        if self.grab_focus && !self.focus.is_focused(window) {
            window.focus(&self.focus, cx);
        }
        let fam = family.clone();
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, w, cx| w.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, _| {
                let k = &ev.keystroke;
                let m = k.modifiers;
                if let Some(bytes) =
                    vt::encode_key(&k.key, k.key_char.as_deref(), m.control, m.alt, m.shift)
                {
                    (this.source.input)(bytes);
                }
            }))
            .child(
                canvas(
                    move |bounds, window, _| {
                        let m = vt::metrics(&fam, px(FONT_SIZE), window);
                        let size = (
                            ((bounds.size.width / m.cell_w) as u32).max(20),
                            ((bounds.size.height / m.line_h) as u32).max(3),
                        );
                        *cells.lock().unwrap_or_else(|e| e.into_inner()) = size;
                    },
                    move |bounds, _, window, cx| {
                        vt::paint(&rows, cursor, &family, px(FONT_SIZE), bounds, window, cx);
                    },
                )
                .size_full(),
            )
    }
}

/// Opens the main window.
pub fn open(l: Launch, cx: &mut App) {
    let opts = gpui::WindowOptions {
        // Full-bleed: no title bar strip; the traffic lights sit over the Workspace rail.
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Synthesis".into()),
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(16.), px(18.))),
        }),
        app_owns_titlebar_drag: true,
        window_decorations: cfg!(target_os = "linux").then_some(gpui::WindowDecorations::Client),
        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            gpui::size(px(1280.), px(800.)),
            cx,
        ))),
        ..Default::default()
    };
    let main = l.screen == Screen::Main;
    #[cfg(feature = "shot")]
    let shot = l.shot.clone();
    let open_arg = l.open.clone();
    let r = cx.open_window(opts, move |window, cx| {
        let view = cx.new(|cx| Shell::new(l, window, cx));
        if main {
            view.update(cx, |v, cx| {
                v.enter_main(cx);
                // development: comma-separated, `diff:PATH`, `session` or a file
                for o in open_arg.iter().flat_map(|o| o.split(',')) {
                    match o.strip_prefix("diff:") {
                        Some(p) => v.open_file(p, true, cx),
                        None if o == "session" => v.open_latest_saved_in_background(cx),
                        None => v.open_file(o, false, cx),
                    }
                }
            });
        }
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        cx.new(|cx| Root::new(view, window, cx))
    });
    match r {
        Err(e) => {
            eprintln!("synthesis-desktop: cannot open the window: {e}");
            cx.quit();
        }
        #[cfg(feature = "shot")]
        Ok(handle) => {
            if let Some((path, secs)) = shot {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(secs))
                        .await;
                    let _ = cx.update_window(handle.into(), |_, window, cx| {
                        // a hidden window gets no frames: draw the latest state before the capture
                        window.refresh();
                        let needed = window.draw(cx);
                        needed.clear(cx);
                        let _ = window.render_to_image();
                        match window.render_to_image().map(|i| i.save(&path)) {
                            Ok(Ok(())) => eprintln!("synthesis-desktop: wrote {}", path.display()),
                            Ok(Err(e)) => eprintln!("synthesis-desktop: png: {e}"),
                            Err(e) => eprintln!("synthesis-desktop: render: {e}"),
                        }
                        cx.quit();
                    });
                })
                .detach();
            }
        }
        #[cfg(not(feature = "shot"))]
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_drawn_shortcuts_and_the_approval_keys_are_bound() {
        let b = super::bindings();
        let keys: Vec<String> = b
            .iter()
            .map(|k| k.keystrokes()[0].key().to_string())
            .collect();
        assert_eq!(keys, ["k", "n", ",", "enter", "backspace", "b"]);
        assert!(b.iter().all(|k| {
            let m = k.keystrokes()[0].modifiers();
            if cfg!(target_os = "macos") {
                m.platform
            } else {
                m.control
            }
        }));
    }
}
