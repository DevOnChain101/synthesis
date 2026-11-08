//! The Settings view (phase 4b): a slide-in panel over the content area, 400 px on wide windows and
//! the full width up to 700 px (the panel of the design's shell, `synthesis-cli-ui/web/shell.css`
//! `#settings`: padding 44/24, 10 px section titles with .18em tracking, 34x20 switches, 6 px radius
//! inputs). The design has no form for these sections, so it is filled with the real schema of
//! `synthesis-settings-ui` (docs/specs/desktop.md section 6). `model` holds all the logic and is
//! tested without a window; this file only paints it.

pub mod model;
#[cfg(feature = "shot")]
pub mod shot;

use std::collections::HashMap;

use gpui::{
    AnyElement, ClickEvent, Context, Entity, EventEmitter, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::*, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use synthesis_settings_ui::{Kind, Origin, Section};
use synthesis_tui_render::theme as t;

use crate::theme::hsla;
use crate::tui_view::layout::Hidden;
use crate::tui_view::tracked;
use model::{Cfg, Change, Control, McpForm, Model, Outcome, Row};

/// Width of the panel (`#settings{width:400px}`) and the window width under which it takes it all.
pub const PANEL_W: f32 = 400.;
pub const NARROW: f32 = 700.;

/// Width of the panel in a content area `avail` wide.
pub fn panel_w(avail: f32) -> f32 {
    if avail <= NARROW {
        avail
    } else {
        PANEL_W.min(avail)
    }
}

/// The dash panels switched off in `synt.tui.panels.*` (applied when the TUI view starts).
pub fn hidden_from(tui: &synthesis_settings::TuiSettings) -> Hidden {
    let mut h = Hidden::default();
    for ((key, _), shown) in synthesis_settings::TUI_PANELS.iter().zip(tui.panels) {
        if !shown {
            h.hide(key);
        }
    }
    h
}

/// `synt.tui.panels.*` changed in the store: the TUI view applies it live.
pub struct PanelsChanged(pub Hidden);

/// The person closed the panel (x button or a click on the dimmed area).
pub struct Close;

fn alpha(c: t::Rgb, a: f32) -> Hsla {
    Hsla { a, ..hsla(c) }
}

fn origin_color(o: Origin) -> t::Rgb {
    match o {
        Origin::Default => t::MUTED,
        Origin::User => t::ACCENT,
        Origin::Project | Origin::Local => t::INFO,
        Origin::Flag => t::CYAN,
        Origin::Managed => t::WARN,
    }
}

fn badge(text: impl Into<SharedString>, c: t::Rgb) -> gpui::Div {
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(9.))
        .border_1()
        .border_color(alpha(c, 0.55))
        .text_size(px(10.))
        .text_color(hsla(c))
        .child(text.into())
}

fn small(text: impl Into<SharedString>, c: t::Rgb) -> gpui::Div {
    div()
        .text_size(px(11.))
        .line_height(px(16.5))
        .text_color(hsla(c))
        .child(text.into())
}

fn btn(id: impl Into<SharedString>, label: &str, primary: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .border_1()
        .border_color(if primary {
            hsla(t::ACCENT)
        } else {
            hsla(t::BORDER)
        })
        .bg(if primary {
            alpha(t::ACCENT, 0.3)
        } else {
            alpha(t::TEXT_STRONG, 0.06)
        })
        .text_size(px(11.5))
        .text_color(hsla(t::TEXT_STRONG))
        .cursor_pointer()
        .hover(|d| d.border_color(hsla(t::ACCENT)))
        .child(label.to_string())
}

fn switch(on: bool, locked: bool) -> gpui::Div {
    div()
        .relative()
        .flex_none()
        .w(px(34.))
        .h(px(20.))
        .rounded(px(10.))
        .border_1()
        .border_color(if on { hsla(t::ACCENT) } else { hsla(t::BORDER) })
        .bg(if on {
            alpha(t::ACCENT, 0.7)
        } else {
            alpha(t::TEXT, 0.14)
        })
        .opacity(if locked { 0.5 } else { 1. })
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(px(if on { 16. } else { 2. }))
                .size(px(14.))
                .rounded_full()
                .bg(if on { hsla(t::BG) } else { alpha(t::TEXT, 0.8) }),
        )
}

pub struct SettingsView {
    model: Option<Model>,
    /// Why there is no model (no home directory).
    err: Option<String>,
    search: Entity<InputState>,
    /// One input per number field and per list editor, by key.
    inputs: HashMap<String, Entity<InputState>>,
    mcp_in: [Entity<InputState>; 5],
    transport: &'static str,
    mcp_error: Option<String>,
    notice: Option<String>,
    scroll: ScrollHandle,
    /// Width taken by chrome beside the view (the shell's rail).
    inset: f32,
    _subs: Vec<Subscription>,
}

impl EventEmitter<Close> for SettingsView {}
impl EventEmitter<PanelsChanged> for SettingsView {}

const MCP_LABELS: [(&str, &str); 5] = [
    ("Name", "docs"),
    ("URL (http / sse)", "https://example.com/mcp"),
    ("Command and arguments (stdio)", "npx -y @scope/server"),
    ("Env KEY=VALUE (optional)", "TOKEN=${DOCS_TOKEN}"),
    ("Header (optional)", "Authorization: Bearer ${DOCS_TOKEN}"),
];

impl SettingsView {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        cfg: Result<Cfg, String>,
        inset: f32,
    ) -> Self {
        let (model, err) = match cfg {
            Ok(c) => (Some(Model::open(c)), None),
            Err(e) => (None, Some(e)),
        };
        let mut subs = vec![];
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        subs.push(
            cx.subscribe_in(&search, window, |this, s, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    if let Some(m) = &mut this.model {
                        m.filter = s.read(cx).value().to_string();
                    }
                    cx.notify();
                }
            }),
        );
        let mut inputs = HashMap::new();
        if let Some(m) = &model {
            for f in &m.store.schema.fields {
                if f.sensitive() && !matches!(f.edit, synthesis_settings_ui::Edit::Confirm(_)) {
                    continue;
                }
                let (placeholder, number) = match f.kind {
                    Kind::Int { .. } | Kind::Float { .. } => ("unset", true),
                    Kind::List(_) => ("add an item, Enter", false),
                    _ => continue,
                };
                let text = match m.row(f).control {
                    Control::Number { text, .. } => text,
                    _ => String::new(),
                };
                let st = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(text)
                });
                let key = f.key.clone();
                subs.push(cx.subscribe_in(
                    &st,
                    window,
                    move |this, s, ev: &InputEvent, window, cx| {
                        let text = s.read(cx).value().to_string();
                        match ev {
                            InputEvent::PressEnter { .. } => {
                                this.commit(&key, number, text, window, cx)
                            }
                            // leaving a number field with an edit commits it; a list keeps the draft
                            InputEvent::Blur if number => {
                                this.commit(&key, number, text, window, cx)
                            }
                            _ => {}
                        }
                    },
                ));
                inputs.insert(f.key.clone(), st);
            }
        }
        let mcp_in =
            MCP_LABELS.map(|(_, ph)| cx.new(|cx| InputState::new(window, cx).placeholder(ph)));
        SettingsView {
            model,
            err,
            search,
            inputs,
            mcp_in,
            transport: "http",
            mcp_error: None,
            notice: None,
            scroll: ScrollHandle::new(),
            inset,
            _subs: subs,
        }
    }

    pub fn model(&self) -> Option<&Model> {
        self.model.as_ref()
    }

    /// Puts the cursor in the search box (the shell does it when the view opens).
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.focus(window, cx));
    }

    fn commit(
        &mut self,
        key: &str,
        number: bool,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let c = if number {
            Change::Text(text.clone())
        } else {
            Change::Add(text.clone())
        };
        if number {
            // an untouched field that lost focus is not a write
            let same = self.model.as_ref().is_some_and(|m| {
                match m.row(m.store.schema.field(key).unwrap()).control {
                    Control::Number { text: cur, .. } => cur == text.trim(),
                    _ => false,
                }
            });
            if same {
                return;
            }
        }
        self.apply(key, c, window, cx);
    }

    fn apply(&mut self, key: &str, c: Change, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let is_add = matches!(c, Change::Add(_));
        let out = m.change(key, c);
        self.after(key, out, is_add, window, cx);
    }

    fn reset(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let out = m.reset(key);
        self.after(key, out, false, window, cx);
    }

    /// The typed text follows the saved value; a refused number keeps what was typed next to its error.
    fn after(
        &mut self,
        key: &str,
        out: Outcome,
        was_add: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = None;
        if out == Outcome::Saved && key.starts_with("synt.tui.panels.") {
            if let Some(m) = &self.model {
                let mut h = Hidden::default();
                for (k, _) in synthesis_settings::TUI_PANELS {
                    let v = m.store.entry(&format!("synt.tui.panels.{k}"));
                    if v.is_some_and(|e| e.value == serde_json::Value::Bool(false)) {
                        h.hide(k);
                    }
                }
                cx.emit(PanelsChanged(h));
            }
        }
        if out == Outcome::Saved {
            let text =
                self.model
                    .as_ref()
                    .and_then(|m| match m.row(m.store.schema.field(key)?).control {
                        Control::Number { text, .. } => Some(text),
                        _ => None,
                    });
            if let Some(inp) = self.inputs.get(key) {
                if was_add {
                    inp.update(cx, |s, cx| s.set_value("", window, cx));
                } else if let Some(text) = text {
                    inp.update(cx, |s, cx| s.set_value(text, window, cx));
                }
            }
        }
        cx.notify();
    }

    fn confirm(&mut self, yes: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        let key = m.pending.as_ref().map(|p| p.key.clone());
        if !yes {
            m.cancel();
        } else if let Some(key) = key {
            let out = m.confirm();
            self.after(&key, out, false, window, cx);
            return;
        }
        cx.notify();
    }

    fn mcp_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v = |i: usize, cx: &mut Context<Self>| self.mcp_in[i].read(cx).value().to_string();
        let form = McpForm {
            name: v(0, cx),
            transport: self.transport.into(),
            url: v(1, cx),
            command: v(2, cx),
            env: v(3, cx),
            header: v(4, cx),
        };
        let Some(m) = &mut self.model else { return };
        match m.add_mcp(&form) {
            Ok(()) => {
                self.mcp_error = None;
                self.notice = Some(format!(
                    "Added MCP server {}. It starts with the next session.",
                    form.name.trim()
                ));
                for i in &self.mcp_in {
                    i.update(cx, |s, cx| s.set_value("", window, cx));
                }
            }
            Err(e) => self.mcp_error = Some(e),
        }
        cx.notify();
    }

    fn mcp_remove(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        match m.remove_mcp(name) {
            Ok(()) => {
                self.mcp_error = None;
                self.notice = Some(format!("Removed MCP server {name}."));
            }
            Err(e) => self.mcp_error = Some(e),
        }
        cx.notify();
    }

    // ---- painting

    fn field_row(&self, r: &Row, cx: &mut Context<Self>) -> AnyElement {
        let key = r.key.clone();
        let locked = r.locked.is_some();
        let sel = SharedString::from(format!("row-{key}"));
        let mut head = div().flex().items_center().gap(px(10.)).child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(hsla(t::TEXT))
                .child(r.label.clone()),
        );
        let mut below: Vec<AnyElement> = vec![];
        match &r.control {
            Control::Switch(on) => {
                let (on, k) = (*on, key.clone());
                let mut s = div()
                    .id(SharedString::from(format!("sw-{key}")))
                    .child(switch(on, locked));
                if !locked {
                    s = s.cursor_pointer().on_click(cx.listener(
                        move |this, _: &ClickEvent, w, cx| this.apply(&k, Change::Bool(!on), w, cx),
                    ));
                }
                head = head.child(s);
            }
            Control::Choice { opts, cur } => {
                let mut seg = div()
                    .flex()
                    .flex_wrap()
                    .gap(px(2.))
                    .p(px(3.))
                    .rounded(px(9.))
                    .bg(alpha(t::TEXT, 0.07));
                for &o in *opts {
                    let (k, v, sel) = (key.clone(), o.to_string(), o == cur);
                    let mut b = div()
                        .id(SharedString::from(format!("opt-{key}-{o}")))
                        .px(px(9.))
                        .py(px(4.))
                        .rounded(px(7.))
                        .text_size(px(11.5))
                        .text_color(if sel {
                            hsla(t::TEXT_STRONG)
                        } else {
                            hsla(t::MUTED)
                        })
                        .when(sel, |d| d.bg(alpha(t::ACCENT, 0.24)))
                        .opacity(if locked { 0.5 } else { 1. })
                        .child(o.to_string());
                    if !locked {
                        b = b.cursor_pointer().on_click(cx.listener(
                            move |this, _: &ClickEvent, w, cx| {
                                this.apply(&k, Change::Choice(v.clone()), w, cx)
                            },
                        ));
                    }
                    seg = seg.child(b);
                }
                below.push(seg.into_any_element());
            }
            Control::Number { range, .. } => {
                if let Some(inp) = self.inputs.get(&key) {
                    below.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(Input::new(inp).disabled(locked)),
                            )
                            .child(small(range.clone(), t::MUTED))
                            .into_any_element(),
                    );
                }
            }
            Control::List(items) => {
                let mut chips = div().flex().flex_wrap().gap(px(5.));
                for (i, it) in items.iter().enumerate() {
                    let k = key.clone();
                    let mut chip = div()
                        .id(SharedString::from(format!("chip-{key}-{i}")))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(hsla(t::BORDER))
                        .text_size(px(11.5))
                        .child(it.clone());
                    if !locked {
                        chip = chip.child(
                            div()
                                .id(SharedString::from(format!("rm-{key}-{i}")))
                                .cursor_pointer()
                                .text_color(hsla(t::MUTED))
                                .hover(|d| d.text_color(hsla(t::ERROR)))
                                .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                    this.apply(&k, Change::Remove(i), w, cx)
                                }))
                                .child("\u{d7}"),
                        );
                    }
                    chips = chips.child(chip);
                }
                if items.is_empty() {
                    chips = chips.child(small("(empty)", t::MUTED));
                }
                below.push(chips.into_any_element());
                if let (Some(inp), false) = (self.inputs.get(&key), locked) {
                    below.push(Input::new(inp).into_any_element());
                }
            }
            Control::Static(v) => {
                head = head.child(
                    div()
                        .max_w(px(180.))
                        .text_size(px(11.5))
                        .text_color(hsla(t::MUTED))
                        .overflow_hidden()
                        .child(v.clone()),
                );
            }
        }
        let mut tags = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .child(badge(model::origin_label(r.origin), origin_color(r.origin)));
        if r.restart {
            tags = tags.child(if r.changed {
                badge("restart required", t::WARN)
            } else {
                badge("restart", t::MUTED)
            });
        }
        if r.confirm {
            tags = tags.child(badge("asks first", t::MUTED));
        }
        if r.resettable {
            let k = key.clone();
            tags = tags.child(
                div()
                    .id(SharedString::from(format!("reset-{key}")))
                    .cursor_pointer()
                    .text_size(px(10.5))
                    .text_color(hsla(t::MUTED))
                    .hover(|d| d.text_color(hsla(t::ACCENT)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| this.reset(&k, w, cx)))
                    .child("reset to default"),
            );
        }
        let mut col = div()
            .debug_selector(move || sel.to_string())
            .flex()
            .flex_col()
            .gap(px(5.))
            .py(px(7.))
            .child(head);
        for b in below {
            col = col.child(b);
        }
        if !r.help.is_empty() {
            col = col.child(small(r.help.clone(), t::MUTED));
        }
        col = col.child(tags);
        if let Some(why) = &r.locked {
            col = col.child(small(format!("Locked: {why}"), t::WARN));
        }
        for x in &r.refused {
            col = col.child(small(x.clone(), t::MUTED));
        }
        if let Some(e) = &r.error {
            col = col.child(small(e.clone(), t::ERROR));
        }
        col.into_any_element()
    }

    fn section_title(&self, s: Section, first: bool) -> AnyElement {
        div()
            .mt(px(if first { 0. } else { 26. }))
            .mb(px(6.))
            .child(tracked(
                &s.title().to_uppercase(),
                10.,
                0.18,
                hsla(t::MUTED),
            ))
            .into_any_element()
    }

    fn info_block(&self, s: Section, m: &Model, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = vec![];
        if let Some(n) = s.note() {
            out.push(small(n, t::MUTED).my(px(4.)).into_any_element());
        }
        match s {
            Section::UserPrompt => {
                out.push(
                    small("Instruction files (AGENTS.md)", t::TEXT)
                        .mt(px(6.))
                        .into_any_element(),
                );
                for (p, there) in &m.agents_md {
                    out.push(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(11.))
                                    .text_color(hsla(t::MUTED))
                                    .child(p.display().to_string()),
                            )
                            .child(badge(
                                if *there { "found" } else { "missing" },
                                if *there { t::SUCCESS } else { t::MUTED },
                            ))
                            .into_any_element(),
                    );
                }
                out.push(
                    small("Provider keys", t::TEXT)
                        .mt(px(10.))
                        .into_any_element(),
                );
                for k in &m.providers {
                    out.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .size(px(7.))
                                    .rounded_full()
                                    .flex_none()
                                    .bg(hsla(if k.set { t::SUCCESS } else { t::MUTED })),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(hsla(t::TEXT))
                                    .child(k.provider.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(11.))
                                    .text_color(hsla(t::MUTED))
                                    .child(k.label.clone()),
                            )
                            .into_any_element(),
                    );
                }
                if let Some(e) = &m.mcp_error {
                    out.push(
                        small(format!("Providers could not be read: {e}"), t::ERROR)
                            .into_any_element(),
                    );
                }
                out.push(
                    small("Keys are environment variables you manage (ADR-011). This view shows whether one is set, never its value, and has no field for a key.", t::MUTED)
                        .mt(px(4.))
                        .into_any_element(),
                );
            }
            Section::Skills => {
                for (n, d, src) in &m.skills {
                    out.push(
                        div()
                            .flex()
                            .flex_col()
                            .py(px(3.))
                            .child(
                                div()
                                    .flex()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(hsla(t::TEXT))
                                            .child(n.clone()),
                                    )
                                    .child(badge(src.clone(), t::MUTED)),
                            )
                            .child(small(d.chars().take(140).collect::<String>(), t::MUTED))
                            .into_any_element(),
                    );
                }
            }
            Section::Mcp => out.extend(self.mcp_block(m, cx)),
            Section::Interface => out.extend(self.panels_block(cx)),
            _ => {}
        }
        out
    }

    /// Hide all / Show all for the panels, and the panels that are never hidden with the reason.
    fn panels_block(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::tui_view::layout::{ALWAYS, HIDEABLE};
        let set_all = |show: bool| {
            cx.listener(move |this, _: &ClickEvent, w, cx| {
                for (k, _) in HIDEABLE {
                    this.apply(&format!("synt.tui.panels.{k}"), Change::Bool(show), w, cx);
                }
            })
        };
        let mut out: Vec<AnyElement> = vec![
            small("Panels", t::TEXT).mt(px(6.)).into_any_element(),
            div()
                .flex()
                .gap(px(8.))
                .my(px(4.))
                .child(btn("panels-hide-all", "Hide all", false).on_click(set_all(false)))
                .child(btn("panels-show-all", "Show all", false).on_click(set_all(true)))
                .into_any_element(),
        ];
        for (name, why) in ALWAYS {
            out.push(small(format!("{name}: {why}"), t::MUTED).into_any_element());
        }
        out
    }

    fn mcp_block(&self, m: &Model, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = vec![
            small("Configured servers", t::TEXT)
                .mt(px(8.))
                .into_any_element(),
        ];
        if m.mcp.is_empty() {
            out.push(small("None configured.", t::MUTED).into_any_element());
        }
        for s in &m.mcp {
            let name = s.name.clone();
            let mut row = div()
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(3.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hsla(t::TEXT_STRONG))
                        .child(s.name.clone()),
                )
                .child(badge(s.transport.clone(), t::MUTED))
                .child(badge(s.scope.clone(), t::MUTED))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(px(11.))
                        .text_color(hsla(t::MUTED))
                        .overflow_hidden()
                        .child(s.target.clone()),
                );
            if let Some(n) = s.tools {
                row = row.child(small(format!("{n} tools"), t::MUTED));
            }
            if s.scope == "user" {
                row = row.child(btn(format!("mcp-rm-{name}"), "Remove", false).on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.mcp_remove(&name, cx)),
                ));
            }
            out.push(row.into_any_element());
        }
        out.push(
            small("Add a server (saved in your user synt.json)", t::TEXT)
                .mt(px(10.))
                .into_any_element(),
        );
        let mut seg = div()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(9.))
            .bg(alpha(t::TEXT, 0.07))
            .w_full();
        for o in ["stdio", "http", "sse"] {
            let on = self.transport == o;
            seg = seg.child(
                div()
                    .id(SharedString::from(format!("mcp-tr-{o}")))
                    .flex_1()
                    .flex()
                    .justify_center()
                    .py(px(4.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .text_color(if on {
                        hsla(t::TEXT_STRONG)
                    } else {
                        hsla(t::MUTED)
                    })
                    .when(on, |d| d.bg(alpha(t::ACCENT, 0.24)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.transport = o;
                        cx.notify();
                    }))
                    .child(o),
            );
        }
        out.push(seg.into_any_element());
        for (i, (label, _)) in MCP_LABELS.iter().enumerate() {
            let stdio = self.transport == "stdio";
            // the field of the other transport is hidden
            if (i == 1 && stdio) || (i == 2 && !stdio) {
                continue;
            }
            out.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .mt(px(6.))
                    .child(small(*label, t::MUTED))
                    .child(Input::new(&self.mcp_in[i]))
                    .into_any_element(),
            );
        }
        out.push(
            small("Secrets are never typed here. Write a ${VAR} reference (for example Authorization: Bearer ${DOCS_TOKEN}); the variable is read when the server starts, and a literal secret is refused.", t::MUTED)
                .mt(px(6.))
                .into_any_element(),
        );
        if let Some(e) = &self.mcp_error {
            out.push(small(e.clone(), t::ERROR).mt(px(4.)).into_any_element());
        }
        out.push(
            div()
                .mt(px(8.))
                .child(
                    btn("mcp-add", "Add server", true)
                        .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.mcp_add(w, cx))),
                )
                .into_any_element(),
        );
        out
    }

    fn confirm_dialog(&self, avail: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.model.as_ref()?.pending.as_ref()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(alpha(t::BG, 0.6))
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.))
                .child(
                    div()
                        .debug_selector(|| "confirm".to_string())
                        .w(px(380_f32.min(avail - 32.)))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .p(px(18.))
                        .rounded(px(12.))
                        .border_1()
                        .border_color(hsla(t::ACCENT))
                        .bg(hsla(t::SURFACE))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(hsla(t::TEXT_STRONG))
                                .child(format!("Change {}?", p.label)),
                        )
                        .child(
                            div()
                                .text_color(hsla(t::WARN))
                                .child(format!("{}.", p.reason.trim_end_matches('.'))),
                        )
                        .child(small(p.key.clone(), t::MUTED))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(8.))
                                .child(btn("confirm-no", "Cancel", false).on_click(cx.listener(
                                    |this, _: &ClickEvent, w, cx| this.confirm(false, w, cx),
                                )))
                                .child(btn("confirm-yes", "Change it", true).on_click(
                                    cx.listener(|this, _: &ClickEvent, w, cx| {
                                        this.confirm(true, w, cx)
                                    }),
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    fn diagnostics(&self, m: &Model) -> Option<AnyElement> {
        let lines: Vec<(String, t::Rgb)> = m
            .store
            .errors
            .iter()
            .map(|e| (e.clone(), t::ERROR))
            .chain(m.diag.iter().map(|d| (d.clone(), t::WARN)))
            .collect();
        if lines.is_empty() {
            return None;
        }
        let mut c = div()
            .debug_selector(|| "diagnostics".to_string())
            .flex_none()
            .max_h(px(110.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(px(2.))
            .mt(px(8.))
            .pt(px(8.))
            .border_t_1()
            .border_color(hsla(t::BORDER))
            .child(small("Diagnostics", t::TEXT));
        for (l, col) in lines {
            c = c.child(small(l, col));
        }
        Some(c.into_any_element())
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let avail = f32::from(window.viewport_size().width) - self.inset;
        let pw = panel_w(avail);
        let pad = if pw < PANEL_W { 16. } else { 24. };
        let mut body = div().flex().flex_col();
        let mut restart_note = None;
        if let Some(m) = &self.model {
            let mut first = true;
            for g in m.groups() {
                body = body.child(self.section_title(g.section, first));
                first = false;
                for r in &g.rows {
                    body = body.child(self.field_row(r, cx));
                }
                if m.filter.trim().is_empty() {
                    for e in self.info_block(g.section, m, cx) {
                        body = body.child(e);
                    }
                }
            }
            if first {
                body = body.child(small("No setting matches the search.", t::MUTED));
            }
            let n = m.restart_pending();
            if n > 0 {
                restart_note = Some(
                    div()
                        .mt(px(10.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(alpha(t::WARN, 0.55))
                        .child(small(
                            format!("{n} change{} take effect in a new session. A running session keeps the old values; restart it from the TUI view (/exit, then start again).", if n == 1 { "" } else { "s" }),
                            t::WARN,
                        )),
                );
            }
        } else if let Some(e) = &self.err {
            body = body.child(small(format!("Settings are unavailable: {e}"), t::ERROR));
        }
        let diag = self.model.as_ref().and_then(|m| self.diagnostics(m));
        let dialog = self.confirm_dialog(pw, cx);

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .mb(px(12.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hsla(t::TEXT_STRONG))
                    .child("Settings"),
            )
            .child(
                div()
                    .id("settings-close")
                    .cursor_pointer()
                    .px(px(6.))
                    .text_size(px(18.))
                    .text_color(hsla(t::MUTED))
                    .hover(|d| d.text_color(hsla(t::TEXT_STRONG)))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(Close)))
                    .child("\u{d7}"),
            );
        let panel = div()
            .debug_selector(|| "settings-panel".to_string())
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(pw))
            .flex()
            .flex_col()
            .pt(px(44.))
            .pb(px(16.))
            .px(px(pad))
            .bg(alpha(t::BG, 0.97))
            .border_l_1()
            .border_color(hsla(t::BORDER))
            .shadow_lg()
            .occlude()
            .child(header)
            .child(
                div()
                    .debug_selector(|| "settings-search".to_string())
                    .child(Input::new(&self.search)),
            )
            .children(restart_note)
            .children(self.notice.clone().map(|n| small(n, t::SUCCESS).mt(px(6.))))
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .mt(px(14.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(body),
            )
            .children(diag)
            .children(dialog);
        div()
            .debug_selector(|| "settings-root".to_string())
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(crate::brand::UI_FONT)
            .text_size(px(13.))
            .child(
                div()
                    .id("settings-scrim")
                    .absolute()
                    .inset_0()
                    .bg(alpha([0, 0, 0], 0.45))
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(Close))),
            )
            .child(panel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, size};
    use synthesis_config::{Env, ProjectTrust};

    #[test]
    fn panel_is_400_wide_and_full_width_under_700() {
        assert_eq!(panel_w(1400.), 400.);
        assert_eq!(panel_w(701.), 400.);
        assert_eq!(panel_w(700.), 700.);
        assert_eq!(panel_w(699.), 699.);
        assert_eq!(panel_w(320.), 320.);
    }

    #[test]
    fn interface_panel_flags_become_hidden_panels() {
        let mut t = synthesis_settings::TuiSettings::default();
        assert_eq!(hidden_from(&t), Hidden::default());
        t.panels[1] = false; // mcp
        t.panels[7] = false; // usage
        let h = hidden_from(&t);
        assert!(h.mcp && h.usage && !h.session && !h.tools);
    }

    fn cfg(dir: &std::path::Path) -> Cfg {
        let root = std::fs::canonicalize(dir).unwrap();
        let (home, cwd) = (root.join("home"), root.join("ws"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let env = Env::from_pairs([
            ("HOME", home.to_string_lossy().into_owned()),
            ("SYNT_HOME", home.to_string_lossy().into_owned()),
        ]);
        Cfg {
            env,
            user_home: home.clone(),
            home,
            cwd,
            managed: root.join("managed.json"),
            trust: ProjectTrust::Untrusted,
        }
    }

    fn open<'a>(
        cx: &'a mut TestAppContext,
        dir: &std::path::Path,
        w: f32,
        h: f32,
    ) -> &'a mut VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let c = cfg(dir);
        let (_, vcx) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        vcx.simulate_resize(size(px(w), px(h)));
        vcx.run_until_parked();
        vcx
    }

    fn b(cx: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("no element {sel}"))
    }

    #[gpui::test]
    fn panel_bounds_at_several_widths(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        for (w, h) in [
            (1400., 900.),
            (1000., 700.),
            (700., 800.),
            (699., 800.),
            (480., 700.),
        ] {
            let v = open(cx, dir.path(), w, h);
            let p = b(v, "settings-panel");
            let want = panel_w(w);
            assert!(
                (f32::from(p.size.width) - want).abs() < 0.6,
                "{w}: panel {} != {want}",
                f32::from(p.size.width)
            );
            assert!(
                (f32::from(p.origin.x) - (w - want)).abs() < 0.6,
                "{w}: panel x"
            );
            assert!(
                (f32::from(p.size.height) - h).abs() < 0.6,
                "{w}: panel height"
            );
            // the search box and the first rows stay inside the panel
            let s = b(v, "settings-search");
            assert!(
                s.origin.x >= p.origin.x && s.right() <= p.right() + px(0.5),
                "{w}: search inside"
            );
            let r = b(v, "row-synt.memory.enabled");
            assert!(
                r.origin.x >= p.origin.x && r.right() <= p.right() + px(0.5),
                "{w}: row inside"
            );
        }
    }

    #[gpui::test]
    fn confirm_dialog_shows_the_reason_and_fits(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        v.simulate_resize(size(px(480.), px(700.)));
        v.update(|window, cx| {
            ent.update(cx, |s, cx| {
                s.apply("synt.memory.reflection", Change::Bool(true), window, cx)
            })
        });
        v.run_until_parked();
        let (d, p) = (b(v, "confirm"), b(v, "settings-panel"));
        assert!(d.origin.x >= p.origin.x && d.right() <= p.right() + px(0.5));
        ent.read_with(v, |s, _| {
            assert!(
                s.model()
                    .unwrap()
                    .pending
                    .as_ref()
                    .unwrap()
                    .reason
                    .starts_with("This sends")
            );
        });
        // cancelling closes it and writes nothing
        v.update(|window, cx| ent.update(cx, |s, cx| s.confirm(false, window, cx)));
        v.run_until_parked();
        assert!(v.debug_bounds("confirm").is_none());
        assert!(!dir.path().join("home/settings.json").exists());
    }

    #[gpui::test]
    fn a_panel_toggle_reaches_the_tui_view_at_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        let seen = std::rc::Rc::new(std::cell::Cell::new(None));
        let s2 = seen.clone();
        v.update(|window, cx| {
            cx.subscribe(&ent, move |_, ev: &PanelsChanged, _| s2.set(Some(ev.0)))
                .detach();
            ent.update(cx, |s, cx| {
                s.apply("synt.tui.panels.usage", Change::Bool(false), window, cx)
            });
        });
        let h = seen.get().expect("PanelsChanged emitted");
        assert!(h.usage && !h.session);
        assert!(dir.path().join("home/settings.json").exists());
    }
}
