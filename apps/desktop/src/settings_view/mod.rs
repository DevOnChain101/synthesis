//! Settings: a centred popup over the TUI view (backdrop, tabs of sections, search, scrolling body), not a view of
//! its own. Every field reads and writes the real USER settings file (`SettingsStore`, atomic write); what the
//! desktop reads applies at once (the shell gets [`Applied`]), the rest says "next session" and offers
//! [`Restart`]. The design has no form for these sections, so it is filled with the real schema of
//! `synthesis-settings-ui` (docs/specs/desktop.md section 6). `model` holds all the logic and is tested without
//! a window; this file only paints it.

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
use gpui_component::scroll::Scrollbar;
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use synthesis_settings_ui::{Kind, Origin, Section};
use synthesis_tui_render::theme as t;

use crate::theme::hsla;
use crate::tui_view::layout::Hidden;
use model::{Cfg, Change, Control, McpForm, Model, Outcome, Row};

/// The popup is 720 to 860 wide (62% of the area), at most 720 high (80%), and keeps a margin to the window.
pub const POPUP_MIN_W: f32 = 720.;
pub const POPUP_MAX_W: f32 = 860.;
pub const POPUP_MAX_H: f32 = 720.;
const MARGIN: f32 = 16.;
/// Below this the popup is as large as the window allows.
const MIN_W: f32 = 280.;
const MIN_H: f32 = 240.;
/// Spacing scale (8 / 12 / 16 / 20), the popup's side padding, and the fixed metrics of the rows and tabs.
const PAD: f32 = 20.;
const TAB_H: f32 = 36.;
const ROW_MIN_H: f32 = 52.;
const NUMBER_W: f32 = 96.;
const RESET_W: f32 = 26.;

/// Size of the popup in a content area `w` x `h`.
pub fn popup_size(w: f32, h: f32) -> (f32, f32) {
    (
        (w * 0.62)
            .clamp(POPUP_MIN_W, POPUP_MAX_W)
            .min(w - 2. * MARGIN)
            .max(MIN_W)
            .min(w),
        (h * 0.8).clamp(MIN_H, POPUP_MAX_H).min(h),
    )
}

/// Width of the column all the controls of a row sit in, right-aligned.
pub fn control_w(pw: f32) -> f32 {
    (pw * 0.36).clamp(168., 280.)
}

/// Focus ring of a control reached with the keyboard.
fn ring(s: gpui::StyleRefinement) -> gpui::StyleRefinement {
    s.border_1().border_color(hsla(t::ACCENT))
}

/// Space or Enter on a focused control.
fn activates(ev: &gpui::KeyDownEvent) -> bool {
    matches!(ev.keystroke.key.as_str(), "space" | "enter")
}

/// A card of rows.
fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(8.))
        .border_1()
        .border_color(hsla(t::BORDER))
        .bg(alpha(t::TEXT, 0.025))
        .overflow_hidden()
}

/// The tabs, in order.
const TABS: [Section; 10] = [
    Section::Appearance,
    Section::Interface,
    Section::Memory,
    Section::Execution,
    Section::Guards,
    Section::Hooks,
    Section::Mcp,
    Section::Skills,
    Section::SubAgents,
    Section::UserPrompt,
];

fn tab_label(s: Section) -> &'static str {
    match s {
        Section::Appearance => "Appearance",
        Section::Interface => "Interface",
        Section::Memory => "Memory",
        Section::Execution => "Execution",
        Section::Guards => "Guards",
        Section::Hooks => "Hooks",
        Section::Mcp => "MCP",
        Section::Skills => "Skills",
        Section::SubAgents => "Sub-agents",
        Section::UserPrompt => "Prompt",
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

/// The person closed the popup (x button, Esc or a click on the backdrop).
pub struct Close;

/// `synt.tui` changed (a saved edit, or a slider being dragged): the shell applies it to the window and the TUI view.
pub struct Applied(pub synthesis_settings::TuiSettings);

/// "Restart session": the shell opens a fresh session, which reads the settings again.
pub struct Restart;

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
    /// The tab strip's own scrolling (it never wraps).
    tab_scroll: ScrollHandle,
    /// The tab shown (a search shows every section).
    tab: Section,
    /// Why "Restart session" cannot be used now (the shell says).
    restart_block: Option<String>,
    /// One slider per percentage field, by key.
    sliders: HashMap<String, Entity<SliderState>>,
    /// Width taken by chrome beside the view (the shell's rail).
    inset: f32,
    _subs: Vec<Subscription>,
}

impl EventEmitter<Close> for SettingsView {}
impl EventEmitter<Applied> for SettingsView {}
impl EventEmitter<Restart> for SettingsView {}

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
        let (mut model, err) = match cfg {
            Ok(c) => (Some(Model::open(c)), None),
            Err(e) => (None, Some(e)),
        };
        if let Some(m) = &mut model {
            m.start_doctor();
            // the checks run on their own thread: look every 250 ms until they are in, then redraw
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                    let Ok(done) = this.update(cx, |v, cx| {
                        let done = v.model.as_mut().is_none_or(|m| m.poll_doctor());
                        if done {
                            cx.notify();
                        }
                        done
                    }) else {
                        break;
                    };
                    if done {
                        break;
                    }
                }
            })
            .detach();
        }
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
        let mut sliders = HashMap::new();
        if let Some(m) = &model {
            for f in &m.store.schema.fields {
                if let Kind::Percent { min, max } = f.kind {
                    let Control::Slider { value, .. } = m.row(f).control else {
                        continue;
                    };
                    let st = cx.new(|_| {
                        SliderState::new()
                            .min(min as f32)
                            .max(max as f32)
                            .step(1.)
                            .default_value(value as f32)
                    });
                    let key = f.key.clone();
                    subs.push(cx.subscribe_in(
                        &st,
                        window,
                        move |this, _, ev: &SliderEvent, window, cx| match ev {
                            // dragging shows the value at once; the file is written when the thumb is released
                            SliderEvent::Change(v) => this.preview(&key, v.end() as u64, cx),
                            SliderEvent::Release(v) => this.apply(
                                &key,
                                Change::Text((v.end().round() as u64).to_string()),
                                window,
                                cx,
                            ),
                        },
                    ));
                    sliders.insert(f.key.clone(), st);
                }
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
            tab_scroll: ScrollHandle::new(),
            tab: TABS[0],
            restart_block: None,
            sliders,
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

    /// `synt.tui` as the file holds it now (what the shell applies).
    fn tui_now(&self) -> synthesis_settings::TuiSettings {
        self.model
            .as_ref()
            .map(|m| synthesis_settings::read_user_tui(&m.cfg.home).0)
            .unwrap_or_default()
    }

    /// The window and the sliders follow the file, not a preview.
    pub(crate) fn show_persisted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = self.tui_now();
        for sl in self.sliders.values() {
            let v = f32::from(t.opacity);
            sl.update(cx, |s, cx| s.set_value(v, window, cx));
        }
        cx.emit(Applied(t));
    }

    /// Closes the popup; a drag that was never released does not stay on the window.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_persisted(window, cx);
        cx.emit(Close);
    }

    /// The shell says whether a restart is possible (`None`) or why not.
    pub fn set_restart_block(&mut self, why: Option<String>, cx: &mut Context<Self>) {
        if self.restart_block != why {
            self.restart_block = why;
            cx.notify();
        }
    }

    /// The fresh session is live: the changes that waited for one are in force now.
    pub fn restarted(&mut self, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.model {
            m.changed.clear();
        }
        self.restart_block = None;
        cx.notify();
    }

    /// A slider being dragged: the window follows, nothing is written yet.
    fn preview(&mut self, key: &str, v: u64, cx: &mut Context<Self>) {
        if key == "synt.tui.opacity" {
            let mut t = self.tui_now();
            t.opacity = v.clamp(u64::from(synthesis_settings::TUI_OPACITY_MIN), 100) as u8;
            cx.emit(Applied(t));
            cx.notify();
        }
    }

    /// Esc: the open confirmation first, then the popup.
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.as_mut().filter(|m| m.pending.is_some()) {
            Some(m) => m.cancel(),
            None => self.close(window, cx),
        }
        cx.notify();
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

    pub(crate) fn apply(
        &mut self,
        key: &str,
        c: Change,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        // a refused or failed write puts the window back to what the file holds (a slider drag had previewed)
        if out != Outcome::Saved && key.starts_with("synt.tui.") {
            self.show_persisted(window, cx);
        }
        if out == Outcome::Saved {
            if key.starts_with("synt.tui.") {
                self.show_persisted(window, cx);
            }
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

    fn trust_revoke(&mut self, root: &str, cx: &mut Context<Self>) {
        let Some(m) = &mut self.model else { return };
        self.notice = Some(match m.revoke_trust(root) {
            Ok(()) => format!("Revoked trust of {root}. It takes effect at the next engine start."),
            Err(e) => format!("Could not revoke: {e}"),
        });
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

    /// One row of a card: label, description and a compact status line on the left; the control, right-aligned
    /// in a column of fixed width, and the reset button on the right. A list editor goes under the row.
    fn field_row(&self, r: &Row, cw: f32, first: bool, cx: &mut Context<Self>) -> AnyElement {
        let key = r.key.clone();
        let locked = r.locked.is_some();
        let sel = SharedString::from(format!("row-{key}"));
        let mut below: Vec<AnyElement> = vec![];
        let mut ctl = div()
            .flex()
            .flex_1()
            .min_w(px(0.))
            .items_center()
            .justify_end()
            .gap(px(8.));
        match &r.control {
            Control::Switch(on) => {
                let (on, k) = (*on, key.clone());
                let k2 = k.clone();
                let mut s = div()
                    .id(SharedString::from(format!("sw-{key}")))
                    .debug_selector({
                        let k = key.clone();
                        move || format!("ctl-{k}")
                    })
                    .rounded(px(10.))
                    .child(switch(on, locked));
                if !locked {
                    s = s
                        .tab_index(0)
                        .focus_visible(ring)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                            this.apply(&k, Change::Bool(!on), w, cx)
                        }))
                        .on_key_down(cx.listener(move |this, ev: &gpui::KeyDownEvent, w, cx| {
                            if activates(ev) {
                                this.apply(&k2, Change::Bool(!on), w, cx);
                                cx.stop_propagation();
                            }
                        }));
                }
                ctl = ctl.child(s);
            }
            Control::Choice { opts, cur } => {
                let mut seg = div()
                    .debug_selector({
                        let k = key.clone();
                        move || format!("ctl-{k}")
                    })
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(2.))
                    .p(px(3.))
                    .rounded(px(9.))
                    .bg(alpha(t::TEXT, 0.07));
                for &o in *opts {
                    let (k, v, on) = (key.clone(), o.to_string(), o == cur);
                    let (k2, v2) = (k.clone(), v.clone());
                    let mut b = div()
                        .id(SharedString::from(format!("opt-{key}-{o}")))
                        .px(px(9.))
                        .py(px(4.))
                        .rounded(px(7.))
                        .text_size(px(11.5))
                        .text_color(if on {
                            hsla(t::TEXT_STRONG)
                        } else {
                            hsla(t::MUTED)
                        })
                        .when(on, |d| d.bg(alpha(t::ACCENT, 0.24)))
                        .opacity(if locked { 0.5 } else { 1. })
                        .child(o.to_string());
                    if !locked {
                        b = b
                            .tab_index(0)
                            .focus_visible(ring)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _: &ClickEvent, w, cx| {
                                this.apply(&k, Change::Choice(v.clone()), w, cx)
                            }))
                            .on_key_down(cx.listener(
                                move |this, ev: &gpui::KeyDownEvent, w, cx| {
                                    if activates(ev) {
                                        this.apply(&k2, Change::Choice(v2.clone()), w, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            ));
                    }
                    seg = seg.child(b);
                }
                ctl = ctl.child(seg);
            }
            Control::Number { range, .. } => {
                if let Some(inp) = self.inputs.get(&key) {
                    ctl = ctl.child(small(range.clone(), t::MUTED)).child(
                        div()
                            .debug_selector({
                                let k = key.clone();
                                move || format!("ctl-{k}")
                            })
                            .w(px(NUMBER_W))
                            .flex_none()
                            .child(Input::new(inp).disabled(locked)),
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
                                .tab_index(0)
                                .focus_visible(ring)
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
            Control::Slider { value, .. } => {
                if let Some(sl) = self.sliders.get(&key) {
                    let now = sl.read(cx).value().end().round() as u64;
                    ctl = ctl.child(
                        div()
                            .debug_selector({
                                let k = key.clone();
                                move || format!("ctl-{k}")
                            })
                            .flex_1()
                            .min_w(px(60.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div().flex_1().min_w(px(40.)).child(
                                    Slider::new(sl)
                                        .horizontal()
                                        .disabled(locked)
                                        .accessibility_label(r.label.clone()),
                                ),
                            )
                            .child(
                                div()
                                    .w(px(40.))
                                    .flex_none()
                                    .flex()
                                    .justify_end()
                                    .child(small(format!("{now} %"), t::TEXT_STRONG)),
                            ),
                    );
                } else {
                    ctl = ctl.child(small(format!("{value} %"), t::MUTED));
                }
            }
            Control::Static(v) => {
                ctl = ctl.child(
                    div()
                        .debug_selector({
                            let k = key.clone();
                            move || format!("ctl-{k}")
                        })
                        .text_size(px(11.5))
                        .text_color(hsla(t::MUTED))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(v.clone()),
                );
            }
        }
        // the reset button: only on a row that was changed; the slot is always there so controls line up
        let reset = div()
            .w(px(RESET_W))
            .flex_none()
            .flex()
            .justify_center()
            .when(r.resettable, |d| {
                let k = key.clone();
                d.child(
                    div()
                        .id(SharedString::from(format!("reset-{key}")))
                        .debug_selector({
                            let k = key.clone();
                            move || format!("reset-{k}")
                        })
                        .tab_index(0)
                        .focus_visible(ring)
                        .p(px(4.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_color(hsla(t::MUTED))
                        .hover(|d| d.text_color(hsla(t::ACCENT)).bg(alpha(t::TEXT, 0.08)))
                        .tooltip({
                            let d = r.default.clone();
                            move |_, cx| -> gpui::AnyView {
                                cx.new(|_| crate::ui::Tip(format!("Reset to default ({d})")))
                                    .into()
                            }
                        })
                        .on_click(cx.listener({
                            let k = k.clone();
                            move |this, _: &ClickEvent, w, cx| this.reset(&k, w, cx)
                        }))
                        .on_key_down(cx.listener(move |this, ev: &gpui::KeyDownEvent, w, cx| {
                            if activates(ev) {
                                this.reset(&k, w, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(crate::widgets::chrome::icon("undo-2", 12., hsla(t::MUTED))),
                )
            });
        // the status line: one badge at most, the default, where the value is set
        let mut status = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .mt(px(3.));
        if r.terminal_only {
            status = status.child(badge("terminal only", t::MUTED));
        } else if r.changed {
            status = status.child(badge("restart needed", t::WARN));
        } else if r.resettable {
            status = status.child(badge("modified", t::ACCENT));
        }
        if !matches!(r.control, Control::Static(_)) {
            status = status.child(small(format!("default: {}", r.default), t::MUTED));
        }
        if r.origin != Origin::Default {
            status = status.child(small(
                format!("set in {}", model::origin_label(r.origin)),
                origin_color(r.origin),
            ));
        }
        if r.confirm {
            status = status.child(small("asks before changing", t::MUTED));
        }
        let mut left = div().flex_1().min_w(px(0.)).flex().flex_col().child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(hsla(t::TEXT_STRONG))
                .child(r.label.clone()),
        );
        if !r.help.is_empty() {
            left = left.child(small(r.help.clone(), t::MUTED).mt(px(2.)));
        }
        left = left.child(status);
        if let Some(why) = &r.locked {
            left = left.child(small(format!("Locked: {why}"), t::WARN).mt(px(2.)));
        }
        for x in &r.refused {
            left = left.child(small(x.clone(), t::MUTED));
        }
        if let Some(e) = &r.error {
            left = left.child(small(e.clone(), t::ERROR).mt(px(3.)));
        }
        let mut col = div()
            .debug_selector(move || sel.to_string())
            .relative()
            .flex()
            .flex_col()
            .px(px(14.))
            .py(px(10.))
            .min_h(px(ROW_MIN_H))
            // the hairline between rows, and a quiet accent on the left edge of a changed row
            .when(!first, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(1.))
                        .bg(alpha(t::TEXT, 0.08)),
                )
            })
            .when(r.resettable, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(2.))
                        .bg(alpha(t::ACCENT, 0.8)),
                )
            })
            .child(
                div().flex().items_center().gap(px(16.)).child(left).child(
                    div()
                        .w(px(cw))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(ctl)
                        .child(reset),
                ),
            );
        for b in below {
            col = col.child(div().mt(px(8.)).child(b));
        }
        col.into_any_element()
    }

    /// A section heading: small caps and a hairline.
    fn heading(&self, text: &str, first: bool) -> AnyElement {
        div()
            .mt(px(if first { 0. } else { 20. }))
            .mb(px(8.))
            .pb(px(6.))
            .border_b_1()
            .border_color(alpha(t::TEXT, 0.10))
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hsla(t::MUTED))
                    .child(text.to_uppercase()),
            )
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
                out.push(small("Doctor", t::TEXT).mt(px(10.)).into_any_element());
                out.push(
                    div()
                        .id("doctor-summary")
                        .text_size(px(11.5))
                        .text_color(hsla(t::TEXT))
                        .child(match (&m.doctor, &m.doctor_error) {
                            (Some(r), _) => synthesis_app::doctor::summary(r),
                            (None, Some(e)) => format!("not checked: {e}"),
                            (None, None) => "checking...".into(),
                        })
                        .into_any_element(),
                );
                for r in m
                    .doctor
                    .iter()
                    .flatten()
                    .filter(|r| r.level != synthesis_app::doctor::Level::Ok)
                {
                    let colour = if r.level == synthesis_app::doctor::Level::Fail {
                        t::ERROR
                    } else {
                        t::WARN
                    };
                    out.push(small(crate::doctor_ui::line(r), colour).into_any_element());
                }
                out.push(small("Read-only. /doctor in the chat lists every check; names of variables are shown, never values.", t::MUTED).into_any_element());
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
            Section::Guards => out.extend(self.trusted_block(m, cx)),
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

    /// The folders remembered as trusted (user store) with a Revoke button each.
    fn trusted_block(&self, m: &Model, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = vec![
            small("Trusted folders", t::TEXT)
                .mt(px(8.))
                .into_any_element(),
        ];
        if m.trusted.is_empty() {
            out.push(
                small(
                    "None remembered. The Files tab offers it for a folder.",
                    t::MUTED,
                )
                .into_any_element(),
            );
        }
        for (i, (r, same)) in m.trusted.iter().enumerate() {
            let root = r.root.clone();
            let state = match (r.scope.as_str(), same) {
                ("parent", _) => "parent: all subfolders",
                (_, Some(true)) => "config unchanged",
                (_, Some(false)) => "CONFIG CHANGED: not honoured",
                _ => "folder missing",
            };
            out.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(3.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(12.))
                            .text_color(hsla(t::TEXT_STRONG))
                            .overflow_hidden()
                            .child(synthesis_harness::sanitize_for_display(&r.root)),
                    )
                    .child(badge(
                        state,
                        if *same == Some(false) {
                            t::WARN
                        } else {
                            t::MUTED
                        },
                    ))
                    .child(small(
                        r.trusted_at.get(..10).unwrap_or(&r.trusted_at).to_string(),
                        t::MUTED,
                    ))
                    .child(btn(format!("trust-rm-{i}"), "Revoke", false).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.trust_revoke(&root, cx)
                        }),
                    ))
                    .into_any_element(),
            );
        }
        if let Some(e) = &m.trust_error {
            out.push(
                small(format!("The trust store could not be read: {e}"), t::ERROR)
                    .into_any_element(),
            );
        }
        if m.trust_unverified > 0 {
            out.push(
                small(
                    format!(
                        "Unverified rows ignored: {} (not written by this app).",
                        m.trust_unverified
                    ),
                    t::WARN,
                )
                .into_any_element(),
            );
        }
        out.push(
            small("Remembered in your session store, never in a project. A remembered trust is deliberately narrower than VS Code: only agents, skills, memory, instructions and the file explorer apply; project hooks, MCP servers, allow rules, env, providers and verification checks need \"Trust for this session\". Revoking takes effect at the next engine start (a new session, /model or a restart); the open session keeps what it loaded.", t::MUTED)
                .mt(px(4.))
                .into_any_element(),
        );
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

impl SettingsView {
    /// The tab strip: one line that never wraps. It scrolls sideways when the popup is too narrow, with a fade at
    /// the edge that hides tabs; the active tab is underlined; Left/Right (strip focused) and Ctrl/Cmd+[ ] switch.
    fn tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let off = self.tab_scroll.offset().x;
        let max = self.tab_scroll.max_offset().x;
        let fade = |left: bool| {
            let (a, b) = (crate::theme::solid(t::SURFACE), alpha(t::SURFACE, 0.));
            let (from, to) = if left { (a, b) } else { (b, a) };
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(28.))
                .when(left, |d| d.left_0())
                .when(!left, |d| d.right_0())
                .bg(gpui::linear_gradient(
                    90.,
                    gpui::linear_color_stop(from, 0.),
                    gpui::linear_color_stop(to, 1.),
                ))
        };
        let mut row = div()
            .id("settings-tabs")
            .debug_selector(|| "tabs".to_string())
            .flex()
            .flex_nowrap()
            .h_full()
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            .gap(px(2.));
        for t in TABS {
            let on = t == self.tab;
            row = row.child(
                div()
                    .id(SharedString::from(format!("tab-{}", tab_label(t))))
                    .debug_selector(move || format!("tab-{}", tab_label(t)))
                    .flex_none()
                    .flex()
                    .items_center()
                    .h_full()
                    .px(px(7.))
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .font_weight(if on {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(hsla(if on { t::TEXT_STRONG } else { t::MUTED }))
                    .border_b_2()
                    .border_color(if on {
                        hsla(t::ACCENT)
                    } else {
                        alpha(t::ACCENT, 0.)
                    })
                    .hover(|d| d.text_color(hsla(t::TEXT_STRONG)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_tab(t, cx)))
                    .child(tab_label(t)),
            );
        }
        div()
            .id("settings-tabs-nav")
            .relative()
            .h(px(TAB_H))
            .flex_none()
            .border_b_1()
            .border_color(alpha(t::TEXT, 0.10))
            .tab_index(0)
            .focus_visible(ring)
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                let step = match ev.keystroke.key.as_str() {
                    "left" => -1,
                    "right" => 1,
                    _ => return,
                };
                this.step_tab(step, cx);
                cx.stop_propagation();
            }))
            .child(row)
            .when(off < px(-1.), |d| d.child(fade(true)))
            .when(max > px(0.) && off > -max + px(1.), |d| {
                d.child(fade(false))
            })
            .into_any_element()
    }

    fn set_tab(&mut self, t: Section, cx: &mut Context<Self>) {
        self.tab = t;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
        if let Some(i) = TABS.iter().position(|x| *x == t) {
            self.tab_scroll.scroll_to_item(i);
        }
        cx.notify();
    }

    /// The tab `by` places further (wrapping round).
    fn step_tab(&mut self, by: isize, cx: &mut Context<Self>) {
        let i = TABS.iter().position(|x| *x == self.tab).unwrap_or(0) as isize;
        self.set_tab(TABS[(i + by).rem_euclid(TABS.len() as isize) as usize], cx);
    }

    /// The body: cards of rows under section headings (the tab's, or every tab's matches while searching).
    fn body(&self, cw: f32, cx: &mut Context<Self>) -> gpui::Div {
        let mut body = div().flex().flex_col();
        let Some(m) = &self.model else {
            if let Some(e) = &self.err {
                body = body.child(small(format!("Settings are unavailable: {e}"), t::ERROR));
            }
            return body;
        };
        let q = m.filter.trim();
        let searching = !q.is_empty();
        let mut first = true;
        for g in m.groups() {
            if !searching && g.section != self.tab {
                continue;
            }
            // rows of one card share a `group`; cards keep the order the schema lists them in
            let mut cards: Vec<(&str, Vec<&Row>)> = vec![];
            for r in &g.rows {
                match cards.iter_mut().find(|(n, _)| *n == r.group) {
                    Some((_, v)) => v.push(r),
                    None => cards.push((r.group, vec![r])),
                }
            }
            for (name, rows) in cards {
                let title = if searching {
                    format!("{} \u{b7} {name}", tab_label(g.section))
                } else {
                    name.to_string()
                };
                body = body.child(self.heading(&title, first));
                first = false;
                let mut card = card();
                for (i, r) in rows.iter().enumerate() {
                    card = card.child(self.field_row(r, cw, i == 0, cx));
                }
                if g.section == Section::Interface && name == "Panels" && !searching {
                    card = card.child(div().p(px(14.)).children(self.panels_block(cx)));
                }
                body = body.child(card);
            }
            if !searching {
                let info = self.info_block(g.section, m, cx);
                if !info.is_empty() {
                    let title = if g.rows.is_empty() {
                        tab_label(g.section)
                    } else {
                        "Details"
                    };
                    body = body.child(self.heading(title, first));
                    first = false;
                    body = body.child(card().p(px(14.)).children(info));
                }
            }
        }
        if first {
            body = body.child(
                div()
                    .debug_selector(|| "no-match".to_string())
                    .py(px(40.))
                    .flex()
                    .justify_center()
                    .child(small(
                        format!("No setting matches \u{201c}{q}\u{201d}."),
                        t::MUTED,
                    )),
            );
        }
        body
    }

    /// The sticky footer: what is waiting for a new session and the button that starts one, or where changes go.
    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let n = self.model.as_ref().map_or(0, |m| m.restart_pending());
        let mut f = div()
            .debug_selector(|| "footer".to_string())
            .flex_none()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(PAD))
            .py(px(10.))
            .border_t_1()
            .border_color(alpha(t::TEXT, 0.10))
            .bg(alpha(t::TEXT, 0.03));
        if n == 0 {
            let path = self
                .model
                .as_ref()
                .map(|m| m.cfg.home.join("settings.json").display().to_string())
                .unwrap_or_default();
            let msg = self
                .notice
                .clone()
                .map(|n| small(n, t::SUCCESS))
                .unwrap_or_else(|| {
                    small(
                        format!("Changes are saved as you make them to {path}."),
                        t::MUTED,
                    )
                });
            return f.child(msg.flex_1().min_w(px(0.))).into_any_element();
        }
        let blocked = self.restart_block.clone();
        let mut msg = div().flex_1().min_w(px(0.)).child(small(
            format!(
                "{n} change{} apply to the next session. The running one keeps the old values until you restart it.",
                if n == 1 { "" } else { "s" }
            ),
            t::WARN,
        ));
        if let Some(why) = &blocked {
            msg = msg.child(
                small(format!("Restart is not possible now: {why}."), t::ERROR)
                    .debug_selector(|| "restart-blocked".to_string()),
            );
        }
        let mut button = btn("restart-session", "Restart session", true);
        button = if blocked.is_some() {
            button.opacity(0.45).cursor_default()
        } else {
            button
                .debug_selector(|| "restart-session".to_string())
                .tab_index(0)
                .focus_visible(ring)
                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(Restart)))
        };
        f = f.child(msg).child(button);
        f.into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let vp = window.viewport_size();
        let (pw, ph) = popup_size(
            f32::from(vp.width) - self.inset,
            f32::from(vp.height) - crate::ui::TITLEBAR_H,
        );
        let cw = control_w(pw);
        let body = self.body(cw, cx);
        let diag = self.model.as_ref().and_then(|m| self.diagnostics(m));
        let dialog = self.confirm_dialog(pw, cx);
        let sw = (pw * 0.36).clamp(140., 300.);

        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(PAD))
            .pt(px(14.))
            .pb(px(12.))
            .child(
                div()
                    .flex_1()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hsla(t::TEXT_STRONG))
                    .child("Settings"),
            )
            .child(
                div()
                    .debug_selector(|| "settings-search".to_string())
                    .w(px(sw))
                    .flex_none()
                    .child(Input::new(&self.search)),
            )
            .child(
                div()
                    .id("settings-close")
                    .tab_index(0)
                    .focus_visible(ring)
                    .rounded(px(6.))
                    .cursor_pointer()
                    .px(px(8.))
                    .text_size(px(18.))
                    .text_color(hsla(t::MUTED))
                    .hover(|d| d.text_color(hsla(t::TEXT_STRONG)))
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.close(w, cx)))
                    .child("\u{d7}"),
            );
        let popup = div()
            .debug_selector(|| "settings-panel".to_string())
            .relative()
            .w(px(pw))
            .h(px(ph))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .bg(crate::theme::solid(t::SURFACE))
            .border_1()
            .border_color(hsla(t::BORDER))
            .shadow_lg()
            .overflow_hidden()
            // the popup swallows clicks; the backdrop under it closes
            .occlude()
            .child(header)
            .child(div().px(px(PAD)).flex_none().child(self.tabs(cx)))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("settings-scroll")
                            .debug_selector(|| "settings-scroll".to_string())
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .px(px(PAD))
                            .pt(px(16.))
                            .pb(px(16.))
                            .child(body),
                    )
                    .child(Scrollbar::vertical(&self.scroll)),
            )
            .children(diag.map(|d| div().px(px(PAD)).pb(px(6.)).child(d)))
            .child(self.footer(cx))
            .children(dialog);
        div()
            .debug_selector(|| "settings-root".to_string())
            .relative()
            .size_full()
            .overflow_hidden()
            .font_family(crate::brand::UI_FONT)
            .text_size(px(13.))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, w, cx| {
                let k = &ev.keystroke;
                match k.key.as_str() {
                    "escape" => this.escape(w, cx),
                    "tab" if k.modifiers.shift => w.focus_prev(cx),
                    "tab" => w.focus_next(cx),
                    "[" | "]" if k.modifiers.secondary(cx) => {
                        this.step_tab(if k.key == "[" { -1 } else { 1 }, cx)
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .id("settings-scrim")
                    .absolute()
                    .inset_0()
                    .bg(alpha([0, 0, 0], 0.5))
                    .on_click(cx.listener(|this, _: &ClickEvent, w, cx| this.close(w, cx))),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(popup),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, size};
    use synthesis_config::{Env, ProjectTrust};

    #[test]
    fn popup_is_720_to_860_wide_at_most_720_high_and_keeps_a_margin() {
        assert_eq!(popup_size(1400., 870.), (860., 696.));
        assert_eq!(popup_size(1000., 670.), (720., 536.));
        assert_eq!(popup_size(700., 600.), (668., 480.));
        // never over the area, even tiny ones
        assert_eq!(popup_size(260., 200.), (260., 200.));
        assert_eq!(control_w(720.), 259.2);
        assert_eq!(control_w(300.), 168.);
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

    fn click(cx: &mut VisualTestContext, sel: &'static str) {
        let at = b(cx, sel).center();
        cx.simulate_click(at, gpui::Modifiers::none());
    }

    fn b(cx: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(sel)
            .unwrap_or_else(|| panic!("no element {sel}"))
    }

    #[gpui::test]
    fn popup_is_centred_inside_the_window_at_several_sizes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        for (w, h) in [
            (1400., 900.),
            (1000., 700.),
            (700., 800.),
            (480., 700.),
            (360., 420.),
        ] {
            let v = open(cx, dir.path(), w, h);
            let p = b(v, "settings-panel");
            let (pw, ph) = popup_size(w, h - crate::ui::TITLEBAR_H);
            let near = |a: Pixels, want: f32, what: &str| {
                assert!(
                    (f32::from(a) - want).abs() < 0.6,
                    "{w}x{h}: {what} {a:?} != {want}"
                )
            };
            near(p.size.width, pw, "width");
            near(p.size.height, ph, "height");
            near(p.origin.x, (w - pw) / 2., "x");
            near(p.origin.y, (h - ph) / 2., "y");
            // the search box, the tabs and the first row stay inside the popup
            let s = b(v, "settings-search");
            assert!(
                s.origin.x >= p.origin.x && s.right() <= p.right() + px(0.5),
                "{w}: search inside"
            );
            let r = b(v, "row-synt.tui.opacity");
            assert!(
                r.origin.x >= p.origin.x && r.right() <= p.right() + px(0.5),
                "{w}: row inside"
            );
        }
    }

    #[gpui::test]
    fn the_tab_strip_never_wraps(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        for w in [360., 480., 720., 760., 800., 860., 1000., 1400.] {
            let v = open(cx, dir.path(), w, 800.);
            let first = b(v, "tab-Appearance");
            for t in TABS {
                let r = b(
                    v,
                    Box::leak(format!("tab-{}", tab_label(t)).into_boxed_str()),
                );
                assert_eq!(
                    r.origin.y,
                    first.origin.y,
                    "{w}: {} on the same line",
                    tab_label(t)
                );
                assert_eq!(r.size.height, first.size.height, "{w}");
            }
            assert!(f32::from(first.size.height) <= TAB_H, "{w}: one line");
            // where all ten fit the popup they are all inside it; narrower, the strip scrolls
            let p = b(v, "settings-panel");
            let last = b(v, "tab-Prompt");
            if w >= 760. {
                assert!(
                    last.right() <= p.right() - px(PAD),
                    "{w}: every tab visible ({:?} vs {:?})",
                    last.right(),
                    p.right()
                );
            }
        }
    }

    #[gpui::test]
    fn controls_share_one_right_edge_and_the_footer_is_never_scrolled(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let v = open(cx, dir.path(), 1000., 800.);
        for (tab, keys) in [
            (
                "tab-Interface",
                vec![
                    "synt.tui.show_thinking",
                    "synt.tui.logo",
                    "synt.tui.verbose",
                ],
            ),
            ("tab-Appearance", vec!["synt.tui.opacity", "synt.tui.blur"]),
            (
                "tab-Memory",
                vec![
                    "synt.memory.enabled",
                    "synt.memory.daily_budget_usd",
                    "synt.images.retentionDays",
                ],
            ),
            ("tab-MCP", vec!["synt.mcp.toolSearch", "synt.mcp.servers"]),
        ] {
            click(v, tab);
            let edges: Vec<f32> = keys
                .iter()
                .map(|k| f32::from(b(v, Box::leak(format!("ctl-{k}").into_boxed_str())).right()))
                .collect();
            assert!(
                edges.windows(2).all(|e| (e[0] - e[1]).abs() < 0.6),
                "{tab}: controls end at one edge {edges:?}"
            );
        }
        // the footer sits under the scroll area, inside the popup, whatever the tab
        let (p, sc, f) = (
            b(v, "settings-panel"),
            b(v, "settings-scroll"),
            b(v, "footer"),
        );
        assert!(sc.bottom() <= f.origin.y + px(0.5) && f.bottom() <= p.bottom() + px(0.5));
        assert!(f32::from(f.size.height) < 80.);
    }

    #[gpui::test]
    fn search_shows_matches_of_every_tab_and_an_empty_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        v.simulate_resize(size(px(1000.), px(800.)));
        let search = |v: &mut VisualTestContext, q: &str| {
            v.update(|_, cx| {
                ent.update(cx, |s, cx| {
                    s.model.as_mut().unwrap().filter = q.into();
                    cx.notify();
                })
            });
            v.run_until_parked();
        };
        // the Appearance tab is open, but a match in Memory shows, and the unrelated opacity row does not
        search(v, "retention");
        assert!(v.debug_bounds("row-synt.images.retentionDays").is_some());
        assert!(v.debug_bounds("row-synt.tui.opacity").is_none());
        assert!(v.debug_bounds("no-match").is_none());
        search(v, "zzzz-nothing");
        assert!(v.debug_bounds("no-match").is_some());
        assert!(v.debug_bounds("footer").is_some(), "the footer stays");
        search(v, "");
        assert!(v.debug_bounds("row-synt.tui.opacity").is_some());
        // the arrow keys of the tab strip step through the tabs and wrap round
        v.update(|_, cx| {
            ent.update(cx, |s, cx| {
                s.step_tab(-1, cx);
                assert_eq!(s.tab, TABS[TABS.len() - 1]);
                s.step_tab(1, cx);
                assert_eq!(s.tab, TABS[0]);
            })
        });
    }

    #[gpui::test]
    fn tabs_show_one_section_and_a_search_shows_all(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let v = open(cx, dir.path(), 1000., 800.);
        // Appearance is the first tab: opacity and blur, not the memory rows
        assert!(v.debug_bounds("row-synt.tui.opacity").is_some());
        assert!(v.debug_bounds("row-synt.tui.blur").is_some());
        assert!(v.debug_bounds("row-synt.memory.enabled").is_none());
        click(v, "tab-Memory");
        assert!(v.debug_bounds("row-synt.memory.enabled").is_some());
        assert!(v.debug_bounds("row-synt.tui.opacity").is_none());
    }

    #[gpui::test]
    fn closing_mid_drag_or_a_failed_write_puts_the_window_back_to_the_file(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        v.simulate_resize(size(px(900.), px(700.)));
        let seen = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        v.update(|window, cx| {
            let s = seen.clone();
            cx.subscribe(&ent, move |_, a: &Applied, _| {
                s.borrow_mut().push(a.0.opacity)
            })
            .detach();
            ent.update(cx, |s, cx| {
                s.apply("synt.tui.opacity", Change::Text("80".into()), window, cx);
                // a drag, then the popup closes before the thumb is released
                s.preview("synt.tui.opacity", 40, cx);
                s.close(window, cx);
                // a write that fails (the file is not JSON): the preview is undone and the row says why
                std::fs::write(dir.path().join("home/settings.json"), "{ nope").unwrap();
                s.model.as_mut().unwrap().store.reload();
                s.preview("synt.tui.opacity", 35, cx);
                s.apply("synt.tui.opacity", Change::Text("35".into()), window, cx);
            })
        });
        // 80 saved, 40 previewed, back to 80 on close, 35 previewed, then the file's value again (100: unreadable file)
        assert_eq!(*seen.borrow(), vec![80, 40, 80, 35, 100]);
        ent.read_with(v, |s, _| {
            let m = s.model().unwrap();
            assert!(m.errors.contains_key("synt.tui.opacity"));
        });
    }

    #[gpui::test]
    fn escape_cancels_a_confirmation_first_then_closes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        v.simulate_resize(size(px(900.), px(700.)));
        let closed = std::rc::Rc::new(std::cell::Cell::new(0));
        v.update(|window, cx| {
            let n = closed.clone();
            cx.subscribe(&ent, move |_, _: &Close, _| n.set(n.get() + 1))
                .detach();
            ent.update(cx, |s, cx| {
                s.focus(window, cx);
                s.apply("synt.memory.reflection", Change::Bool(true), window, cx)
            })
        });
        v.run_until_parked();
        assert!(v.debug_bounds("confirm").is_some());
        v.simulate_keystrokes("escape");
        assert!(
            v.debug_bounds("confirm").is_none(),
            "the dialog is cancelled"
        );
        assert_eq!(closed.get(), 0, "and the popup stays");
        v.simulate_keystrokes("escape");
        assert_eq!(closed.get(), 1);
    }

    #[gpui::test]
    fn a_saved_tui_key_is_applied_live_and_a_session_key_asks_for_a_restart(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (ent, v) = cx.add_window_view(|window, cx| SettingsView::new(window, cx, Ok(c), 0.));
        v.simulate_resize(size(px(900.), px(700.)));
        let seen = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
        let restarts = std::rc::Rc::new(std::cell::Cell::new(0));
        v.update(|window, cx| {
            let (s, r) = (seen.clone(), restarts.clone());
            cx.subscribe(&ent, move |_, a: &Applied, _| s.borrow_mut().push(a.0))
                .detach();
            cx.subscribe(&ent, move |_, _: &Restart, _| r.set(r.get() + 1))
                .detach();
            ent.update(cx, |s, cx| {
                s.apply("synt.tui.opacity", Change::Text("60".into()), window, cx);
                s.apply("synt.tui.motion", Change::Bool(false), window, cx);
                // a drag shows the value without writing it
                s.preview("synt.tui.opacity", 45, cx);
                s.apply("synt.memory.enabled", Change::Bool(false), window, cx);
            })
        });
        let seen = seen.borrow().clone();
        assert_eq!(seen.len(), 3, "the memory key is not a window setting");
        assert_eq!((seen[0].opacity, seen[0].motion), (60, true));
        assert_eq!((seen[1].opacity, seen[1].motion), (60, false));
        assert_eq!((seen[2].opacity, seen[2].motion), (45, false));
        let on_disk = synthesis_settings::read_user_tui(&dir.path().join("home")).0;
        assert_eq!(on_disk.opacity, 60, "the preview wrote nothing");
        // the memory change waits for a session: the banner has a working button. Clicking only asks the shell:
        // nothing is cleared and the popup stays until the shell says the session was restarted
        v.run_until_parked();
        click(v, "restart-session");
        assert_eq!(restarts.get(), 1);
        ent.read_with(v, |s, _| {
            assert_eq!(s.model().unwrap().restart_pending(), 1)
        });
        // the shell refuses (a task is running): the button is off and says why
        v.update(|_, cx| {
            ent.update(cx, |s, cx| {
                s.set_restart_block(Some("a task is running".into()), cx)
            })
        });
        v.run_until_parked();
        assert!(v.debug_bounds("restart-blocked").is_some());
        assert!(
            v.debug_bounds("restart-session").is_none(),
            "no live button"
        );
        // the new session is live: now the badge goes
        v.update(|_, cx| ent.update(cx, |s, cx| s.restarted(cx)));
        ent.read_with(v, |s, _| {
            assert_eq!(s.model().unwrap().restart_pending(), 0)
        });
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
        let _g = crate::theme::opacity_guard();
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
            cx.subscribe(&ent, move |_, ev: &Applied, _| s2.set(Some(ev.0)))
                .detach();
            ent.update(cx, |s, cx| {
                s.apply("synt.tui.panels.usage", Change::Bool(false), window, cx)
            });
        });
        let t: synthesis_settings::TuiSettings = seen.get().expect("Applied emitted");
        let h = hidden_from(&t);
        assert!(h.usage && !h.session);
        assert!(dir.path().join("home/settings.json").exists());
    }
}
