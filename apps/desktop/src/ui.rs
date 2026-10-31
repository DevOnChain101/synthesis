//! The window: title bar, sidebar and workspace skeleton after `design/03-designs/desktop/desktop.html`,
//! the onboarding screen, Settings (mode selector, Appearance font picker) and the Mode 1 terminal
//! pane. Logic lives in `onboarding`, `settings`, `credentials` and `vt`; this file only draws it.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{
    ActiveTheme as _, Root, Sizable as _, StyledExt as _, TitleBar, h_flex, v_flex,
};
use synthesis_config::Env;

use crate::credentials::CredentialStore;
use crate::onboarding::{self, Kind, Outcome};
use crate::settings::{self, DesktopSettings, FontChoice, Mode};
use crate::theme::{self, hsla};
use crate::vt;

const FONT_SIZE: f32 = 13.0;

/// Registers the bundled OFL fonts (apps/desktop/assets/fonts) with GPUI.
pub fn register_fonts(cx: &App) {
    let fonts: [&'static [u8]; 3] = [
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
        include_bytes!("../assets/fonts/FiraCode-Regular.ttf"),
        include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf"),
    ];
    if let Err(e) = cx
        .text_system()
        .add_fonts(fonts.into_iter().map(Cow::Borrowed).collect())
    {
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

pub struct Shell {
    screen: Screen,
    nav: Nav,
    side_tab: SideTab,
    settings: DesktopSettings,
    home: PathBuf,
    project: String,
    env: Env,
    store: Arc<dyn CredentialStore>,
    rt: Arc<tokio::runtime::Runtime>,
    form: Form,
    term: Entity<TermPane>,
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

impl Shell {
    #[allow(clippy::too_many_arguments)] // one construction site; a params struct adds nothing
    pub fn new(
        screen: Screen,
        nav: Nav,
        home: PathBuf,
        project: PathBuf,
        env: Env,
        store: Arc<dyn CredentialStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = Kind::Anthropic;
        let form = Form {
            kind,
            id: input(window, cx, "provider id", kind.default_id(), false),
            url: input(window, cx, "https://…", kind.default_url(), false),
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
                "",
                false,
            ),
            status: None,
            busy: false,
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map(Arc::new)
            .expect("tokio runtime");
        let term = cx.new(TermPane::new);
        let project = project.file_name().map_or_else(
            || project.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Self {
            screen,
            nav,
            side_tab: SideTab::Sessions,
            settings: settings::load(&home),
            home,
            project,
            env,
            store,
            rt,
            form,
            term,
        }
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

    fn test_connection(&mut self, cx: &mut Context<Self>) {
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
        cx.spawn(async move |this, cx| {
            let outcome = rx.await.unwrap_or(Outcome::Unreachable);
            let _ = this.update(cx, |this, cx| {
                this.form.busy = false;
                if let Outcome::Ok { models } = &outcome
                    && let Some(m) = models.first()
                    && this.form.model.read(cx).value().is_empty()
                {
                    let m = m.clone();
                    // set_value needs a window; the model field is filled on the next click instead
                    this.form.status =
                        Some((false, format!("{} First model: {m}", outcome.text())));
                } else {
                    this.form.status =
                        Some((!matches!(outcome, Outcome::Ok { .. }), outcome.text()));
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
        if let Err(e) = self.store.set(&valid.id, valid.key) {
            self.form.status = Some((true, e.to_string()));
            return cx.notify();
        }
        self.form
            .key
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.form.status = None;
        self.screen = Screen::Main;
        cx.notify();
    }
}

// ---------------------------------------------------------------------------------- drawing

fn label(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(px(10.))
        .text_color(hsla(theme::MUTED))
        .child(text.into().to_uppercase())
}

fn panel(title: &str, sub: &str) -> gpui::Div {
    v_flex()
        .overflow_hidden()
        .border_1()
        .border_color(hsla(0x303640))
        .rounded(px(14.))
        .bg(hsla(0x15191E))
        .child(
            h_flex()
                .h(px(45.))
                .px(px(16.))
                .justify_between()
                .border_b_1()
                .border_color(hsla(0x2D333B))
                .text_size(px(13.))
                .child(div().font_semibold().child(title.to_string()))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(hsla(0x9DA4AE))
                        .child(sub.to_string()),
                ),
        )
}

fn empty(text: &str) -> gpui::Div {
    div()
        .p(px(16.))
        .text_size(px(12.))
        .text_color(hsla(theme::MUTED))
        .child(text.to_string())
}

fn choice(id: impl Into<gpui::ElementId>, text: &str, selected: bool) -> Button {
    let b = Button::new(id).label(text.to_string()).small();
    if selected { b.primary() } else { b.outline() }
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
                                Button::new("test")
                                    .label("Test connection")
                                    .outline()
                                    .loading(f.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                            )
                            .child(
                                Button::new("save")
                                    .label("Save and continue")
                                    .primary()
                                    .on_click(cx.listener(|this, _, w, cx| this.save(w, cx))),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Sidebar 1, `Workspace`: the rail of workspaces (projects). One row today: the folder the
    /// app was started in.
    fn workspace_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let rule = || div().h(px(1.)).my(px(8.)).bg(hsla(0x30333A));
        let row = |id: &'static str, glyph: &str, text: String, active: bool| {
            let r = h_flex()
                .id(id)
                .gap(px(12.))
                .p(px(10.))
                .my(px(2.))
                .rounded(px(9.))
                .text_size(px(13.))
                .text_color(hsla(if active { 0xFFFFFF } else { 0xAAB0BA }))
                .child(
                    div()
                        .w(px(16.))
                        .text_color(hsla(0xB6BDC6))
                        .child(glyph.to_string()),
                )
                .child(text);
            r.when_active(active).cursor_pointer()
        };
        v_flex()
            .w(px(210.))
            .flex_shrink_0()
            .h_full()
            .justify_between()
            .p(px(13.))
            .bg(hsla(theme::SIDEBAR))
            .border_r_1()
            .border_color(hsla(0x2C3139))
            .child(
                v_flex()
                    .child(
                        div()
                            .m(px(9.))
                            .mb(px(16.))
                            .text_size(px(12.))
                            .child("SYNTHESIS"),
                    )
                    .child(
                        Button::new("new-task")
                            .label("New Task        ⌘N")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.nav = Nav::Session;
                                cx.notify();
                            })),
                    )
                    .child(rule())
                    .child(row("search", "⌕", "Search        ⌘K".into(), false))
                    .child(row("inbox", "▤", "Inbox".into(), false))
                    .child(rule())
                    .child(
                        h_flex()
                            .justify_between()
                            .px(px(10.))
                            .child(label("Workspace"))
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(hsla(theme::MUTED))
                                    .child("+"),
                            ),
                    )
                    .child(row("ws-current", "▣", self.project.clone(), true).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.nav = Nav::Session;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                v_flex()
                    .child(
                        row(
                            "config",
                            "⚙",
                            "Configuration".into(),
                            self.nav == Nav::Configuration,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav = Nav::Configuration;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .px(px(10.))
                            .pt(px(6.))
                            .text_size(px(11.))
                            .text_color(hsla(theme::MUTED))
                            .child(format!(
                                "v{} · {}",
                                env!("CARGO_PKG_VERSION"),
                                match (self.settings.mode, self.settings.plain_terminal) {
                                    (Mode::Gpui, _) => "GPUI",
                                    (Mode::Tui, false) => "TUI",
                                    (Mode::Tui, true) => "TUI · plain terminal",
                                }
                            )),
                    ),
            )
            .into_any_element()
    }

    /// Sidebar 2, `Sessions`, for the selected workspace: tabs Sessions | Explorer | Changes.
    fn sessions_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = |id: &'static str, text: &'static str, t: SideTab| {
            let on = self.side_tab == t;
            div()
                .id(id)
                .flex_1()
                .py(px(6.))
                .text_center()
                .rounded(px(7.))
                .text_size(px(12.))
                .text_color(hsla(if on { 0xFFFFFF } else { theme::MUTED }))
                .cursor_pointer()
                .when(on, |d| d.bg(hsla(theme::NAV_ACTIVE)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.side_tab = t;
                    cx.notify();
                }))
                .child(text)
        };
        let page = |id: &'static str, text: &'static str, n: Nav| {
            div()
                .id(id)
                .text_size(px(12.))
                .text_color(hsla(if self.nav == n {
                    0xFFFFFF
                } else {
                    theme::MUTED
                }))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav = n;
                    cx.notify();
                }))
                .child(text)
        };
        let body = match self.side_tab {
            SideTab::Sessions => v_flex()
                .gap(px(8.))
                .child(
                    div()
                        .p(px(9.))
                        .rounded(px(9.))
                        .border_1()
                        .border_color(hsla(0x30333A))
                        .text_size(px(12.))
                        .text_color(hsla(theme::MUTED))
                        .child("⌕  Search sessions…"),
                )
                .child(label("Sessions"))
                .child(empty("No sessions yet."))
                .child(
                    Button::new("new-session")
                        .label("+ New session")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav = Nav::Session;
                            cx.notify();
                        })),
                ),
            SideTab::Explorer => v_flex().child(empty("The project file tree appears here.")),
            SideTab::Changes => v_flex().child(empty("Changed files appear here.")),
        };
        v_flex()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .gap(px(10.))
            .p(px(13.))
            .bg(hsla(theme::SURFACE))
            .border_r_1()
            .border_color(hsla(0x2C3139))
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_semibold()
                            .child(self.project.clone()),
                    )
                    .child(div().text_color(hsla(theme::MUTED)).child("+")),
            )
            .child(
                h_flex()
                    .p(px(3.))
                    .rounded(px(9.))
                    .bg(hsla(theme::BG))
                    .child(tab("t-sessions", "Sessions", SideTab::Sessions))
                    .child(tab("t-explorer", "Explorer", SideTab::Explorer))
                    .child(tab("t-changes", "Changes", SideTab::Changes)),
            )
            .child(
                h_flex()
                    .gap(px(12.))
                    .child(page("p-activity", "Activity", Nav::Activity))
                    .child(page("p-tools", "Tools", Nav::Tools))
                    .child(page("p-topology", "Agent Topology", Nav::Topology)),
            )
            .child(body)
            .into_any_element()
    }

    /// Main area: session tab, conversation, composer, file/diff viewer on the right, terminal
    /// panel at the bottom, status line.
    fn session(&self) -> AnyElement {
        let tab = |t: &str, on: bool| {
            div()
                .px(px(14.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(hsla(if on { 0xFFFFFF } else { theme::MUTED }))
                .when(on, |d| d.border_b_1().border_color(hsla(theme::PEARL)))
                .child(t.to_string())
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .border_b_1()
                    .border_color(hsla(0x2D333B))
                    .child(tab("New session", true)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        panel("Session", "No task running")
                            .flex_1()
                            .m(px(12.))
                            .child(empty("Describe a goal in the composer to start a task."))
                            .child(
                                div()
                                    .mt_auto()
                                    .m(px(12.))
                                    .p(px(15.))
                                    .border_1()
                                    .border_color(hsla(theme::INPUT_BORDER))
                                    .rounded(px(11.))
                                    .bg(hsla(0x232831))
                                    .text_size(px(13.))
                                    .text_color(hsla(0x9DA6B2))
                                    .child("Ask Synthesis or add acceptance criteria…  ↗"),
                            ),
                    )
                    .child(
                        panel("File / Diff", "Code · Diff")
                            .w(px(380.))
                            .my(px(12.))
                            .mr(px(12.))
                            .child(empty("Open a file or a change to see it here.")),
                    ),
            )
            .child(
                panel("Terminal", "Terminal · Browser · Verification")
                    .h(px(200.))
                    .mx(px(12.))
                    .child(empty("No terminal yet.")),
            )
            .child(
                h_flex()
                    .px(px(16.))
                    .py(px(6.))
                    .justify_between()
                    .text_size(px(11.))
                    .text_color(hsla(theme::MUTED))
                    .child("0 tokens")
                    .child("needs you: 0"),
            )
            .into_any_element()
    }

    fn page(&self, title: &str) -> AnyElement {
        v_flex()
            .size_full()
            .p(px(24.))
            .gap(px(12.))
            .child(div().text_size(px(22.)).child(title.to_string()))
            .child(empty(
                "This page arrives in a later phase (docs/specs/desktop.md section 7).",
            ))
            .into_any_element()
    }

    fn settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = self.settings;
        let section = |t: &str| div().mt(px(8.)).text_size(px(15.)).child(t.to_string());
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
            .p(px(24.))
            .gap(px(12.))
            .child(div().text_size(px(22.)).child("Configuration"))
            .child(section("Interface"))
            .child(div().text_size(px(12.)).text_color(hsla(theme::MUTED)).child(
                "TUI draws the terminal interface in this window; GPUI is the native interface.",
            ))
            .child(h_flex().gap(px(8.)).children(modes))
            .child(section("Appearance"))
            .child(div().text_size(px(12.)).text_color(hsla(theme::MUTED)).child(
                "Font for code, terminal and chat. Monospace is the default; fonts are bundled (SIL OFL).",
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
                .border_1()
                .border_color(hsla(0x45474B))
        } else {
            self
        }
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.screen {
            Screen::Onboarding => self.onboarding(cx),
            Screen::Main => {
                let content = match (self.nav, self.settings.mode) {
                    (Nav::Configuration, _) => self.settings_page(cx),
                    (Nav::Activity, _) => self.page("Activity"),
                    (Nav::Tools, _) => self.page("Tools"),
                    (Nav::Topology, _) => self.page("Agent Topology"),
                    (Nav::Session, Mode::Tui) => self.term.clone().into_any_element(),
                    (Nav::Session, Mode::Gpui) => self.session(),
                };
                h_flex()
                    .size_full()
                    .child(self.workspace_rail(cx))
                    .child(self.sessions_panel(cx))
                    .child(div().flex_1().h_full().child(content))
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .bg(hsla(theme::BG))
            .text_color(hsla(theme::TEXT))
            .font_family(self.font())
            .child(
                TitleBar::new().child(
                    div()
                        .flex_1()
                        .text_center()
                        .text_size(px(12.))
                        .text_color(hsla(theme::MUTED))
                        .child(format!("Synthesis — {}", self.project)),
                ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

// ------------------------------------------------------------------------- Mode 1 terminal pane

/// The TUI in a terminal grid. D0: the TUI runs in this process against an in-memory terminal with
/// a scripted model; D2 replaces the model with the real engine.
pub struct TermPane {
    grid: vt::Grid,
    rig: Option<crate::d0::Rig>,
    cells: Arc<Mutex<(u32, u32)>>,
    focus: FocusHandle,
}

impl TermPane {
    fn new(cx: &mut Context<Self>) -> Self {
        let rig = crate::d0::start(
            100,
            30,
            "D0 demo: this answer comes from a scripted model; the TUI runs inside this window.",
        );
        let cells = rig.size.clone();
        let mut pane = Self {
            grid: vt::Grid::new(100, 30),
            rig: None,
            cells,
            focus: cx.focus_handle(),
        };
        let mut rig = rig;
        let mut bytes = std::mem::replace(&mut rig.bytes, futures::channel::mpsc::unbounded().1);
        pane.rig = Some(rig);
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
        let rows: Vec<_> = (0..self.grid.size().1).map(|i| self.grid.runs(i)).collect();
        let cursor = self.grid.cursor();
        let family = cx.theme().mono_font_family.clone();
        let cells = self.cells.clone();
        if !self.focus.is_focused(window) {
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
                if let (Some(bytes), Some(rig)) = (
                    vt::encode_key(&k.key, k.key_char.as_deref(), m.control, m.alt, m.shift),
                    &this.rig,
                ) {
                    let _ = rig.keys.send(bytes);
                }
            }))
            .child(
                canvas(
                    move |bounds, window, _| {
                        let m = vt::metrics(&fam, px(FONT_SIZE), window);
                        let size = (
                            ((bounds.size.width / m.cell_w) as u32).max(20),
                            ((bounds.size.height / m.line_h) as u32).max(5),
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
pub fn open(
    screen: Screen,
    nav: Nav,
    home: PathBuf,
    project: PathBuf,
    env: Env,
    store: Arc<dyn CredentialStore>,
    cx: &mut App,
) {
    let opts = gpui::WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            gpui::size(px(1280.), px(800.)),
            cx,
        ))),
        ..Default::default()
    };
    let r = cx.open_window(opts, move |window, cx| {
        let view = cx.new(|cx| Shell::new(screen, nav, home, project, env, store, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    if let Err(e) = r {
        eprintln!("synthesis-desktop: cannot open the window: {e}");
        cx.quit();
    }
}
