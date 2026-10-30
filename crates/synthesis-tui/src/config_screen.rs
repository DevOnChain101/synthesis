//! `/config`: the rows, the keys and the live edits of [`TuiSettings`] (spec
//! docs/specs/tui-design.md §7). The screen only changes the in-memory settings; the app writes them
//! once, when the screen closes.

use synthesis_tui_base::keys::{Key, Named};
use synthesis_tui_render::config_view::{ConfigView, GROUPS, Row, State};
use synthesis_tui_render::layout::{PanelId, REGISTRY, Side};
use synthesis_tui_render::theme::ThemeId;

use crate::tui_settings::{GlyphChoice, LogoChoice, TuiSettings};

pub(crate) struct ConfigScreen {
    pub view: ConfigView,
    /// `NO_COLOR` is set: the theme is Mono and cannot be changed.
    no_color: bool,
    /// What the settings were when the screen opened (is there anything to save?).
    before: TuiSettings,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Stay,
    /// A setting changed: the next frame is the preview.
    Changed,
    Close,
}

const PAGE: usize = 8;

fn locked_desc(id: PanelId) -> &'static str {
    if id == PanelId::Subagents {
        "always on the right"
    } else {
        "always visible"
    }
}

/// Every row of every group, in display order.
pub(crate) fn rows(s: &TuiSettings, no_color: bool) -> Vec<Row> {
    let mut out = vec![];
    let sections = [
        ("Left sidebar", [Side::Left].as_slice()),
        ("Centre", &[Side::Chat, Side::CentreTop, Side::CentreTab]),
        ("Right sidebar", &[Side::Right]),
    ];
    for (name, sides) in sections {
        for sp in REGISTRY.iter().filter(|p| sides.contains(&p.side)) {
            out.push(Row {
                group: 0,
                section: name,
                id: format!("panels.{}", sp.key),
                label: sp.title.to_string(),
                desc: if sp.locked.is_some() {
                    locked_desc(sp.id).into()
                } else {
                    sp.desc.into()
                },
                state: State::Toggle(s.panels.is_on(sp.id)),
                lock: sp.locked.map(String::from),
                panel: Some(sp.id),
            });
        }
    }
    let row = |group, id: &str, label: &str, desc: &str, state, lock: Option<&str>| Row {
        group,
        section: "",
        id: id.into(),
        label: label.into(),
        desc: desc.into(),
        state,
        lock: lock.map(String::from),
        panel: None,
    };
    let toggle = |b| State::Toggle(b);
    out.extend([
        row(
            1,
            "theme",
            "Theme",
            "colour theme (NO_COLOR forces mono)",
            State::Choice(if no_color {
                "mono".into()
            } else {
                s.theme_name().into()
            }),
            no_color.then_some("NO_COLOR is set: the theme is mono."),
        ),
        row(
            1,
            "glyphs",
            "Glyphs",
            "unicode or ascii symbols",
            State::Choice(s.glyphs_name().into()),
            None,
        ),
        row(
            1,
            "logo",
            "Logo",
            "welcome screen art",
            State::Choice(s.logo_name().into()),
            None,
        ),
        row(
            2,
            "chat.show_thinking",
            "Show thinking",
            "model thinking in the chat (applies from P3)",
            toggle(s.show_thinking),
            None,
        ),
        row(
            2,
            "chat.diffs_expanded",
            "Expand diffs",
            "show diffs unfolded (applies from P3)",
            toggle(s.diffs_expanded),
            None,
        ),
        row(
            2,
            "chat.queue_indicators",
            "Queue indicators",
            "+n queued above the prompt (applies from P3)",
            toggle(s.queue_indicators),
            None,
        ),
        row(
            2,
            "motion",
            "Motion",
            "animated spinners (applies from P4)",
            toggle(s.motion),
            None,
        ),
        row(
            3,
            "mouse.capture",
            "Mouse capture",
            "off keeps the terminal's own selection (applies from P4)",
            toggle(s.mouse_capture),
            None,
        ),
        row(
            3,
            "mouse.copy_on_select",
            "Copy on select",
            "copy a selection when the button is released (applies from P4)",
            toggle(s.copy_on_select),
            None,
        ),
    ]);
    out
}

fn next<T: Copy + PartialEq>(all: &[T], cur: T, back: bool) -> T {
    let i = all.iter().position(|c| *c == cur).unwrap_or(0);
    all[if back {
        (i + all.len() - 1) % all.len()
    } else {
        (i + 1) % all.len()
    }]
}

impl ConfigScreen {
    pub fn new(s: &TuiSettings, no_color: bool, diagnostics: &[String]) -> Self {
        Self {
            view: ConfigView {
                rows: rows(s, no_color),
                hint: diagnostics.first().cloned(),
                ..Default::default()
            },
            no_color,
            before: *s,
        }
    }

    /// Settings differ from what they were on opening.
    pub fn changed(&self, now: &TuiSettings) -> bool {
        *now != self.before
    }

    fn refresh(&mut self, s: &TuiSettings) {
        self.view.rows = rows(s, self.no_color);
    }

    fn focused(&self) -> Option<usize> {
        self.view.visible().get(self.view.cursor).copied()
    }

    fn clamp_cursor(&mut self) {
        let n = self.view.visible().len();
        self.view.cursor = self.view.cursor.min(n.saturating_sub(1));
    }

    /// Space / Enter / click on the focused row: toggle it, cycle it, or say why it cannot change.
    fn activate(&mut self, s: &mut TuiSettings, back: bool) -> Step {
        let Some(i) = self.focused() else {
            return Step::Stay;
        };
        let row = self.view.rows[i].clone();
        if let Some(reason) = row.lock {
            self.view.hint = Some(reason);
            return Step::Stay;
        }
        self.view.hint = None;
        let id = row.id.as_str();
        if let Some(key) = id.strip_prefix("panels.") {
            let Some(p) = PanelId::parse(key) else {
                return Step::Stay;
            };
            s.panels.set(p, !s.panels.is_on(p));
        } else {
            match id {
                "theme" => {
                    let all: Vec<ThemeId> = ThemeId::CHOICES.iter().map(|c| c.1).collect();
                    s.theme = next(&all, s.theme, back);
                }
                "glyphs" => {
                    let all: Vec<GlyphChoice> = GlyphChoice::CHOICES.iter().map(|c| c.1).collect();
                    s.glyphs = next(&all, s.glyphs, back);
                }
                "logo" => {
                    let all: Vec<LogoChoice> = LogoChoice::CHOICES.iter().map(|c| c.1).collect();
                    s.logo = next(&all, s.logo, back);
                }
                "chat.show_thinking" => s.show_thinking = !s.show_thinking,
                "chat.diffs_expanded" => s.diffs_expanded = !s.diffs_expanded,
                "chat.queue_indicators" => s.queue_indicators = !s.queue_indicators,
                "motion" => s.motion = !s.motion,
                "mouse.capture" => s.mouse_capture = !s.mouse_capture,
                "mouse.copy_on_select" => s.copy_on_select = !s.copy_on_select,
                _ => return Step::Stay,
            }
        }
        self.refresh(s);
        Step::Changed
    }

    /// `r`: the defaults for the group of the focused row.
    fn reset(&mut self, s: &mut TuiSettings) -> Step {
        let Some(i) = self.focused() else {
            return Step::Stay;
        };
        let d = TuiSettings::default();
        match self.view.rows[i].group {
            0 => s.panels = d.panels,
            1 => (s.theme, s.glyphs, s.logo) = (d.theme, d.glyphs, d.logo),
            2 => {
                (
                    s.show_thinking,
                    s.diffs_expanded,
                    s.queue_indicators,
                    s.motion,
                ) = (
                    d.show_thinking,
                    d.diffs_expanded,
                    d.queue_indicators,
                    d.motion,
                )
            }
            _ => (s.mouse_capture, s.copy_on_select) = (d.mouse_capture, d.copy_on_select),
        }
        self.view.hint = None;
        self.refresh(s);
        Step::Changed
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.view.visible().len();
        self.view.cursor =
            (self.view.cursor as isize + delta).clamp(0, n.saturating_sub(1) as isize) as usize;
        self.view.hint = None;
    }

    fn group(&mut self, back: bool) {
        let n = GROUPS.len();
        self.view.group = (self.view.group + if back { n - 1 } else { 1 }) % n;
        self.view.cursor = 0;
        self.view.hint = None;
    }

    /// Click on listed row `k` (index into the visible rows): focus it and activate it.
    pub fn click(&mut self, k: usize, s: &mut TuiSettings) -> Step {
        if k >= self.view.visible().len() {
            return Step::Stay;
        }
        self.view.cursor = k;
        self.activate(s, false)
    }

    /// Wheel: move the cursor.
    pub fn scroll(&mut self, down: bool) {
        self.move_by(if down { 3 } else { -3 });
    }

    pub fn key(&mut self, k: &Key, s: &mut TuiSettings) -> Step {
        let on_choice = self
            .focused()
            .is_some_and(|i| matches!(self.view.rows[i].state, State::Choice(_)));
        let v = &mut self.view;
        match k {
            Key::Named(Named::Escape) => {
                if v.filtering || !v.filter.is_empty() {
                    v.filtering = false;
                    v.filter.clear();
                    self.clamp_cursor();
                    return Step::Stay;
                }
                return Step::Close;
            }
            Key::Named(Named::Up) => self.move_by(-1),
            Key::Named(Named::Down) => self.move_by(1),
            Key::Named(Named::PageUp) => self.move_by(-(PAGE as isize)),
            Key::Named(Named::PageDown) => self.move_by(PAGE as isize),
            Key::Named(Named::Home) => self.move_by(-10_000),
            Key::Named(Named::End) => self.move_by(10_000),
            Key::Named(Named::Tab) => self.group(false),
            Key::Named(Named::ShiftTab) => self.group(true),
            // ← → choose within a choice row; on a toggle space and enter are the keys
            Key::Named(n @ (Named::Left | Named::Right)) if on_choice => {
                return self.activate(s, *n == Named::Left);
            }
            Key::Named(Named::Enter) => {
                if v.filtering {
                    v.filtering = false;
                } else {
                    return self.activate(s, false);
                }
            }
            Key::Named(Named::Backspace) if v.filtering => {
                v.filter.pop();
                self.clamp_cursor();
            }
            Key::Text(t) if v.filtering => {
                v.filter.push_str(t);
                v.cursor = 0;
            }
            Key::Text(t) if t == " " => return self.activate(s, false),
            // `/` starts the filter (a burst such as `/mot` also carries the first letters)
            Key::Text(t) if t.starts_with('/') => {
                v.filtering = true;
                v.filter.push_str(&t[1..]);
                v.cursor = 0;
            }
            Key::Text(t) if t == "r" => return self.reset(s),
            _ => {}
        }
        Step::Stay
    }
}
