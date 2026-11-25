//! The "Source Control" section of the file explorer (owner request, like VS Code): the branch, the staged and the
//! changed files with a letter each, stage / unstage per file, Stage all, and a one-line commit. The git work is
//! `synthesis_view::project::scm` (hardened; read its docs): it runs on a background task, never on the UI
//! thread, and re-reads the state at the click. A click on a row opens that file's diff in the viewer. No push,
//! pull, fetch, discard or branch operation exists here. WSL mode: not available yet (like the diff).

use std::path::PathBuf;

use gpui::{
    AnyView, Context, Div, Entity, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::project::scm::{self, Act, Entry, Scm};

use super::TuiView;
use super::panels::{dim, strong, text, with_alpha};
use crate::theme::hsla;
use crate::ui::Tip;
use crate::widgets::codeblock::icon_btn;
use crate::widgets::panes::note;

/// What the explorer keeps of the Source Control section (in `TabState`).
#[derive(Default)]
pub struct State {
    /// The section is open (it starts collapsed).
    pub open: bool,
    /// The Files section is collapsed (it starts open).
    pub files_shut: bool,
    pub data: Option<Result<Scm, String>>,
    /// What the last action said: git's error, or what was skipped.
    pub note: Option<String>,
    /// An action is running: a second click waits.
    pub busy: bool,
    /// The commit message box, made when the section is first opened (it needs the window).
    pub msg: Option<Entity<InputState>>,
}

/// The letter shown for a file (`?` is "untracked": U, as in VS Code) and its colour.
fn shown(c: char) -> (char, t::Rgb) {
    match c {
        '?' => ('U', t::SUCCESS),
        '!' => ('!', t::ERROR),
        c => (c, super::explorer_git::badge_colour(c)),
    }
}

impl TuiView {
    /// The section header of the explorer: chevron, title, and what goes at the right.
    pub(super) fn section_header(
        &self,
        id: &'static str,
        title: String,
        open: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(6.))
            .border_b_1()
            .border_color(hsla(t::RULE))
            .cursor_pointer()
            .text_color(dim())
            .hover(|d| d.text_color(strong()))
            .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
            .child(div().w(px(10.)).text_size(px(10.)).child(if open {
                "\u{25BE}"
            } else {
                "\u{25B8}"
            }))
            .child(div().flex_1().child(title))
    }

    /// The Files section's title: `FILES`, or what the tree is doing.
    pub(super) fn files_title(&self) -> String {
        if self.tree.loading {
            "reading...".into()
        } else {
            "FILES".into()
        }
    }

    pub(super) fn toggle_files(&mut self, cx: &mut Context<Self>) {
        self.ts.scm.files_shut = !self.ts.scm.files_shut;
        cx.notify();
    }

    pub(super) fn toggle_scm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ts.scm.open = !self.ts.scm.open;
        if self.ts.scm.open {
            if self.ts.scm.msg.is_none() {
                let e = cx.new(|cx| InputState::new(window, cx).placeholder("Message"));
                // Enter does not commit: the input does not say whether an IME composition is open, and a
                // commit is not something to trigger by accident; the button is the way
                cx.subscribe_in(&e, window, |_, _, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                })
                .detach();
                self.ts.scm.msg = Some(e);
            }
            self.request_scm(cx);
        }
        cx.notify();
    }

    /// Reads the repository state off the UI thread. Same epoch guard as the tree.
    pub(super) fn request_scm(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        if lu.live.engine.as_ref().is_some_and(|e| e.is_remote()) {
            self.ts.scm.data = Some(Err("not available in WSL mode yet".into()));
            return;
        }
        let (root, env, trusted) = (
            PathBuf::from(&lu.live.cwd),
            lu.cfg.env.clone(),
            lu.live.trusted,
        );
        let (cwd, epoch) = (lu.live.cwd.clone(), self.tree.epoch);
        let job = cx.background_spawn(async move { scm::status(&root, &env, trusted) });
        cx.spawn(async move |this, cx| {
            let d = job.await;
            this.update(cx, |t, cx| {
                let same =
                    t.tree.epoch == epoch && t.live.as_ref().is_some_and(|l| l.live.cwd == cwd);
                if same {
                    t.ts.scm.data = Some(d);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// One click: stage, unstage, stage all or commit, off the UI thread; then the lists are read again (also
    /// when it failed: the state is the truth) and what git said is shown.
    pub(super) fn scm_act(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        if self.ts.scm.busy || lu.live.engine.as_ref().is_some_and(|e| e.is_remote()) {
            return;
        }
        let (root, env, trusted) = (
            PathBuf::from(&lu.live.cwd),
            lu.cfg.env.clone(),
            lu.live.trusted,
        );
        let (cwd, epoch) = (lu.live.cwd.clone(), self.tree.epoch);
        let commit = matches!(act, Act::Commit(_));
        self.ts.scm.busy = true;
        self.ts.scm.note = None;
        let job = cx.background_spawn(async move { scm::apply(&root, &env, trusted, &act) });
        cx.spawn_in(window, async move |this, cx| {
            let r = job.await;
            this.update_in(cx, |t, window, cx| {
                t.ts.scm.busy = false; // also when the session or folder changed meanwhile
                let same =
                    t.tree.epoch == epoch && t.live.as_ref().is_some_and(|l| l.live.cwd == cwd);
                if !same {
                    cx.notify();
                    return;
                }
                match r {
                    Ok(n) => {
                        t.ts.scm.note = Some(n).filter(|n| !n.is_empty());
                        if let (true, Some(m)) = (commit, &t.ts.scm.msg) {
                            m.update(cx, |m, cx| m.set_value("", window, cx));
                        }
                    }
                    Err(why) => t.ts.scm.note = Some(why),
                }
                t.request_scm(cx);
                t.request_scan(cx);
                if let Some(p) = t.ts.tree_sel.clone() {
                    t.request_file_diff(p, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// A click on a row: that file's diff, in the viewer.
    fn scm_open(&mut self, path: String, cx: &mut Context<Self>) {
        self.ts.egit.file_mode = false;
        self.select_file(&path, cx);
    }

    /// One file of a group: letter, path (the diff opens on a click), and its stage / unstage button.
    fn scm_row(&self, i: usize, e: &Entry, staged: bool, cx: &mut Context<Self>) -> Div {
        let (letter, colour) = shown(e.letter);
        let sel = self.ts.tree_sel.as_deref() == Some(e.path.as_str());
        let (open, act, busy) = (e.path.clone(), e.path.clone(), self.ts.scm.busy);
        let (id, icon, tip) = if staged {
            (("scm-unstage", i), "minus", "Unstage")
        } else if e.secret {
            (
                ("scm-stage", i),
                "plus",
                "Looks like a secret file: not staged",
            )
        } else {
            (("scm-stage", i), "plus", "Stage")
        };
        let btn = icon_btn(
            format!("{}-{i}", id.0).into(),
            icon,
            tip.into(),
            if !staged && e.secret {
                with_alpha(dim(), 0.4)
            } else {
                dim()
            },
            None,
        )
        .when(busy || (!staged && e.secret), |b| b.opacity(0.5))
        .on_click(cx.listener(move |this, _, window, cx| {
            let a = if staged {
                Act::Unstage(act.clone())
            } else {
                Act::Stage(act.clone())
            };
            this.scm_act(a, window, cx);
        }));
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(22.))
            .rounded(px(3.))
            .when(sel, |d| d.bg(hsla(t::ELEVATED)))
            .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.05)))
            .child(
                div()
                    .id(("scm-row", i + if staged { 0 } else { 10_000 }))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_1()
                    .min_w(px(0.))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.scm_open(open.clone(), cx)))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(hsla(colour))
                            .child(letter.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(if sel { strong() } else { text() })
                            .child(synthesis_harness::sanitize_for_display(&e.path)),
                    ),
            )
            .child(btn)
    }

    /// A group's header (`Staged Changes  3`) with its bulk button, then its rows.
    fn scm_group(&self, title: &str, files: &[Entry], staged: bool, cx: &mut Context<Self>) -> Div {
        let head = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(22.))
            .text_color(dim())
            .text_size(px(11.))
            .child(div().flex_1().child(title.to_string()))
            .when(!staged && !files.is_empty(), |d| {
                d.child(
                    icon_btn(
                        "scm-stage-all".into(),
                        "plus",
                        "Stage all (secret-looking files are skipped)".into(),
                        dim(),
                        None,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.scm_act(Act::StageAll, window, cx)),
                    ),
                )
            })
            .child(
                div()
                    .px(px(6.))
                    .rounded_full()
                    .bg(with_alpha(hsla(t::TEXT_STRONG), 0.10))
                    .child(files.len().to_string()),
            );
        div().flex().flex_col().child(head).children(
            files
                .iter()
                .enumerate()
                .map(|(i, e)| self.scm_row(i, e, staged, cx)),
        )
    }

    /// The body of the open section: message box and Commit, the note, the two groups.
    pub(super) fn scm_body(&self, cx: &mut Context<Self>) -> Div {
        let s = &self.ts.scm;
        let body = div().flex().flex_col().gap(px(6.)).p(px(8.));
        let (head, d) = match &s.data {
            None => return body.child(note("reading...")),
            Some(Err(why)) => return body.child(note(why.clone())),
            Some(Ok(d)) => (d.head.clone(), d),
        };
        let msg = s
            .msg
            .as_ref()
            .map(|m| m.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let can = !d.staged.is_empty() && !msg.is_empty() && !s.busy;
        let commit = div()
            .id("scm-commit")
            .flex()
            .justify_center()
            .py(px(4.))
            .rounded(px(4.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .text_color(if can { strong() } else { dim() })
            .when(can, |b| {
                b.cursor_pointer()
                    .hover(|b| b.bg(hsla(t::ELEVATED)))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let m = this
                            .ts
                            .scm
                            .msg
                            .as_ref()
                            .map(|m| m.read(cx).value().to_string())
                            .unwrap_or_default();
                        this.scm_act(Act::Commit(m), window, cx);
                    }))
            })
            .when(!can, |b| b.opacity(0.5))
            .tooltip(move |_, cx| -> AnyView {
                cx.new(|_| Tip("Commit the staged changes (git hooks are not run)".into()))
                    .into()
            })
            .child(format!("Commit ({})", d.staged.len()));
        body.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(dim())
                .child(crate::widgets::chrome::icon("git-branch", 13., dim()))
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(text())
                        .child(synthesis_harness::sanitize_for_display(&head)),
                ),
        )
        .children(s.msg.as_ref().map(|m| {
            div()
                .px(px(6.))
                .rounded(px(4.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .child(Input::new(m).appearance(false).bordered(false))
        }))
        .child(commit)
        .children(s.note.as_ref().map(|n| {
            div()
                .id("scm-note")
                .text_color(hsla(t::ERROR))
                .whitespace_normal()
                .child(n.clone())
        }))
        .child(self.scm_group("Staged Changes", &d.staged, true, cx))
        .child(self.scm_group("Changes", &d.changes, false, cx))
        .when(d.cut, |b| {
            b.child(
                div()
                    .text_color(dim())
                    .child("\u{2026} cut at the display limit"),
            )
        })
    }

    /// The title of the section header: `SOURCE CONTROL` and the number of changed files.
    pub(super) fn scm_title(&self) -> String {
        match &self.ts.scm.data {
            Some(Ok(d)) if self.ts.scm.open => {
                format!("SOURCE CONTROL  {}", d.staged.len() + d.changes.len())
            }
            _ => "SOURCE CONTROL".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_colours_follow_the_explorer_badges() {
        assert_eq!(shown('?'), ('U', t::SUCCESS));
        assert_eq!(shown('M'), ('M', t::WARN));
        assert_eq!(shown('D'), ('D', t::ERROR));
        assert_eq!(shown('A'), ('A', t::SUCCESS));
        assert_eq!(shown('!').1, t::ERROR);
    }

    #[test]
    fn the_section_starts_collapsed_and_files_open() {
        let s = State::default();
        assert!(!s.open && !s.files_shut && !s.busy);
    }
}
