//! The kind picker `+ new` (and `n`) opens (`docs/specs/super-agent-s1.md` section 1, phase P1): New session,
//! New goal (writes), New goal (read-only). Nothing is preselected and Enter does nothing until a row is chosen
//! (arrows, 1-3 or a click). The goal rows only enter the existing outcome form; the read-only row is shown but
//! disabled until the engine advertises `read_only_goal` (`orchestrator.status` features, set by P2b).
//! Row, hover and keyboard styling follows the Super Agent views' flat theme tokens (no borders on rows).

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::outcomes::WSL2_REFUSAL;

use super::OutcomesView;
use super::model::{Effect, Screen, State};
use crate::theme::hsla;
use crate::widgets::chrome::{black_a, dim, strong, text, with_alpha};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Session,
    Goal,
    ReadOnlyGoal,
}

pub const ROWS: [Kind; 3] = [Kind::Session, Kind::Goal, Kind::ReadOnlyGoal];

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "New session",
            Self::Goal => "New goal (writes)",
            Self::ReadOnlyGoal => "New goal (read-only)",
        }
    }
    pub fn blurb(self) -> &'static str {
        match self {
            Self::Session => "A plain chat with an agent. A folder is optional.",
            Self::Goal => {
                "Criteria and checks, worked in a git worktree; done when the checks pass."
            }
            Self::ReadOnlyGoal => {
                "Research or review that sends back a report. Nothing is written."
            }
        }
    }
}

/// The open picker: the row the cursor is on (`None`: nothing chosen) and why the last choice did not go on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picker {
    pub cursor: Option<usize>,
    pub notice: Option<String>,
}

pub const READ_ONLY_SOON: &str =
    "Read-only goals need an engine that advertises read_only_goal; this one does not.";
const NOT_READY: &str = "Goals need Super Agent turned on and connected.";

/// Why `kind` cannot be started now (the row is then shown disabled), or `None`.
pub fn refusal(kind: Kind, screen: &Screen, read_only_goal: bool) -> Option<&'static str> {
    match (kind, screen) {
        (Kind::Session, _) => None,
        (_, Screen::Windows) => Some(WSL2_REFUSAL),
        (Kind::ReadOnlyGoal, _) if !read_only_goal => Some(READ_ONLY_SOON),
        (_, Screen::Ready) => None,
        _ => Some(NOT_READY),
    }
}

impl State {
    pub fn open_picker(&mut self) {
        self.ws_menu = None;
        self.picker = Some(Picker::default());
    }

    pub fn pick(&mut self, i: usize) {
        if let Some(p) = self.picker.as_mut().filter(|_| i < ROWS.len()) {
            p.cursor = Some(i);
            p.notice = None;
        }
    }

    /// Enter or a click on the chosen row. With no row chosen it does nothing.
    pub fn pick_activate(&mut self) -> Vec<Effect> {
        let Some(i) = self.picker.as_ref().and_then(|p| p.cursor) else {
            return vec![];
        };
        let kind = ROWS[i];
        if let Some(why) = refusal(kind, &self.screen, self.read_only_goal) {
            if let Some(p) = self.picker.as_mut() {
                p.notice = Some(why.into());
            }
            return vec![];
        }
        self.picker = None;
        match kind {
            Kind::Session => vec![Effect::NewSession],
            _ => self.open_new(),
        }
    }

    pub(crate) fn picker_key(&mut self, name: &str) -> Vec<Effect> {
        let at = self.picker.as_ref().and_then(|p| p.cursor);
        match name {
            "escape" => self.picker = None,
            "up" => self.pick(at.map_or(0, |i| i.saturating_sub(1))),
            "down" => self.pick(at.map_or(0, |i| (i + 1).min(ROWS.len() - 1))),
            "enter" => return self.pick_activate(),
            n => {
                if let Some(d) = n
                    .parse::<usize>()
                    .ok()
                    .filter(|d| (1..=ROWS.len()).contains(d))
                {
                    self.pick(d - 1);
                }
            }
        }
        vec![]
    }
}

/// A fresh `<synt home>/scratch/<id>` folder (mode 0700 on unix) for a session with no folder. It lives under
/// the synt home only; a `scratch` that is a symlink (or not a folder) is refused, and an existing id is never reused.
pub fn make_scratch(synt_home: &Path) -> Result<PathBuf, String> {
    let parent = synt_home.join("scratch");
    let mk = |p: &Path, recursive: bool| {
        let mut b = std::fs::DirBuilder::new();
        b.recursive(recursive);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(p)
    };
    if let Err(e) = mk(&parent, true)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(format!("cannot make the scratch folder: {e}"));
    }
    // checked after creating, on the entry itself: a link here would send the session outside the synt home
    match std::fs::symlink_metadata(&parent) {
        Ok(m) if m.is_dir() => {}
        _ => return Err("the scratch folder is a link or not a folder; it was not used".into()),
    }
    let dir = parent.join(uuid::Uuid::new_v4().simple().to_string());
    mk(&dir, false).map_err(|e| format!("cannot make the session folder: {e}"))?;
    Ok(dir)
}

impl OutcomesView {
    pub(super) fn picker_overlay(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let p = self.state.picker.as_ref()?;
        let rows = ROWS.iter().enumerate().map(|(i, k)| {
            let off = refusal(*k, &self.state.screen, self.state.read_only_goal);
            let on = p.cursor == Some(i);
            let main = div()
                .id(("out-kind", i))
                .debug_selector(move || format!("out-kind-{i}"))
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(8.))
                .when(on, |d| d.bg(hsla(t::ELEVATED)))
                .when(off.is_none(), |d| {
                    d.cursor_pointer()
                        .hover(|d| d.bg(with_alpha(hsla(t::ELEVATED), 0.7)))
                })
                .on_click(self.click(cx, move |this, _| this.state.pick(i)))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if off.is_some() { dim() } else { strong() })
                        .child(div().text_color(dim()).child((i + 1).to_string()))
                        .child(k.label()),
                )
                .child(
                    div()
                        .pl(px(16.))
                        .text_size(px(11.))
                        .text_color(dim())
                        .child(off.unwrap_or(k.blurb())),
                );
            div()
                .flex()
                .items_center()
                .child(main)
                .children((*k == Kind::Session).then(|| {
                    crate::outcomes_view::form_view::small_button(
                        "out-kind-folder",
                        "Choose folder...",
                        self.click(cx, |this, cx| this.session_in_folder(cx)),
                    )
                }))
        });
        let panel = div()
            .debug_selector(|| "out-kind-picker".to_string())
            .w(px(520.))
            .max_w_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(10.))
            .rounded(px(10.))
            .bg(crate::theme::fill(t::SURFACE))
            .child(
                div()
                    .px(px(12.))
                    .pb(px(4.))
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(strong())
                    .child("New"),
            )
            .children(rows)
            .children(p.notice.as_ref().map(|n| {
                div()
                    .debug_selector(|| "out-kind-notice".to_string())
                    .px(px(12.))
                    .pt(px(4.))
                    .text_color(hsla(t::WARN))
                    .child(synthesis_harness::sanitize_for_display(n))
            }))
            .child(
                div()
                    .px(px(12.))
                    .pt(px(6.))
                    .text_size(px(11.))
                    .text_color(dim())
                    .child(
                        "Up/Down, 1-3 or a click choose, Enter starts the chosen row, Esc closes.",
                    ),
            );
        Some(
            div()
                .debug_selector(|| "out-kind-scrim".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.))
                .text_color(text())
                .bg(black_a(0.55))
                .child(panel)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_private_unique_and_under_the_home() {
        let home = tempfile::tempdir().unwrap();
        let (a, b) = (
            make_scratch(home.path()).unwrap(),
            make_scratch(home.path()).unwrap(),
        );
        assert_ne!(a, b);
        assert!(a.starts_with(home.path().join("scratch")) && a.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for d in [&a, &b, &home.path().join("scratch")] {
                assert_eq!(d.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_scratch_parent_is_refused() {
        let (home, elsewhere) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::os::unix::fs::symlink(elsewhere.path(), home.path().join("scratch")).unwrap();
        assert!(make_scratch(home.path()).is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}
