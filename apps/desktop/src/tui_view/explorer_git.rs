//! Git in the file explorer: the badge of a changed file (and the marker a folder shows for what is inside it),
//! and the diff of the selected file against HEAD with a file/diff switch in the viewer. The git work is
//! `synthesis_view::project` (hardened, bounded, see its docs); this runs it on a background task, never on the
//! UI thread. A secret file or a diff with secret-shaped text is never shown. WSL mode: the badges come with the
//! engine's tree; the diff is not available there yet (no per-file `fs.git_diff` call), and says so.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui::{
    Context, Div, InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::project::gutter::{diff_signs, sign_marks};
use synthesis_view::project::{self, FileDiff, row_paths};

use super::TuiView;
use super::panels::{dim, strong, with_alpha};
use super::tabs::read_preview;
use crate::theme::hsla;
use crate::widgets::codeblock::icon_btn;
use crate::widgets::panes::{git_line, mark_bar, note};

/// What the viewer keeps of the git side of the explorer (in `TabState`).
#[derive(Default)]
pub struct State {
    /// The person chose the file's text; the default for a changed file is its diff.
    pub file_mode: bool,
    pub diff: Option<(String, Result<FileDiff, String>)>,
}

/// The colour of a status letter: M and R amber, A and untracked green, D and conflicted red.
pub fn badge_colour(c: char) -> t::Rgb {
    match c {
        'A' | '?' => t::SUCCESS,
        'D' | 'U' => t::ERROR,
        _ => t::WARN,
    }
}

/// How much a letter matters when a folder shows one marker for everything inside it.
fn rank(c: char) -> u8 {
    match c {
        'U' => 5,
        'D' => 4,
        'M' => 3,
        'R' => 2,
        'A' => 1,
        _ => 0,
    }
}

/// The letter each folder shows for the changes inside it (ignored entries do not count): from the git status
/// of the whole repository where there is one, and from the rows listed (the engine's tree in WSL mode).
pub fn folder_marks(d: &project::ProjectData) -> HashMap<String, char> {
    let mut out: HashMap<String, char> = HashMap::new();
    let paths = row_paths(d);
    let rows = d
        .rows
        .iter()
        .zip(&paths)
        .filter_map(|(r, p)| r.status.map(|c| (p.as_str(), c)));
    let all = d.listing.status.iter().map(|(p, c)| (p.as_str(), *c));
    for (p, c) in rows.chain(all).filter(|(_, c)| *c != '!') {
        for (i, _) in p.match_indices('/') {
            let e = out.entry(p[..i].to_string()).or_insert(c);
            if rank(c) > rank(*e) {
                *e = c;
            }
        }
    }
    out
}

impl TuiView {
    /// The status letter of the file `path` of the tree.
    fn file_status(&self, path: &str) -> Option<char> {
        let d = self.tree.data.as_ref()?;
        let row = d.rows.iter().zip(row_paths(d)).find(|(_, p)| p == path);
        match row {
            Some((r, _)) if r.dir => None,
            Some((r, _)) => r.status,
            // a file of a folder not opened in the tree (a click in Source Control): the status of the repository
            None => d.listing.status.get(path).copied(),
        }
        .filter(|c| *c != '!')
    }

    /// Reads the diff of `path` off the UI thread (a click on a file, Refresh). Same epoch guard as the preview.
    pub(super) fn request_file_diff(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        let Some(letter) = self.file_status(&path) else {
            self.ts.egit.diff = None;
            return;
        };
        if lu.live.engine.as_ref().is_some_and(|e| e.is_remote()) {
            self.ts.egit.diff = Some((
                path,
                Err("the diff is not available in WSL mode yet".into()),
            ));
            return;
        }
        let (root, env, trusted) = (
            PathBuf::from(&lu.live.cwd),
            lu.cfg.env.clone(),
            lu.live.trusted,
        );
        let epoch = self.tree.epoch;
        let rel = path.clone();
        let job = cx.background_spawn(async move {
            if letter == '?' {
                // not in git yet: all of it is added; read as the preview reads (no links out, no secrets)
                let b = read_preview(&root, &rel)?;
                project::all_added(&root, &rel, &b.lines)
            } else {
                project::file_diff(&root, &env, trusted, &rel)
            }
        });
        cx.spawn(async move |this, cx| {
            let body = job.await;
            this.update(cx, |t, cx| {
                if t.tree.epoch == epoch && t.ts.tree_sel.as_deref() == Some(path.as_str()) {
                    t.ts.egit.diff = Some((path, body));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The viewer: the selected file as `preview_view` draws it, or, for a changed file, a file/diff switch over
    /// its diff (the default).
    pub(super) fn viewer(&self, cx: &mut Context<Self>) -> Div {
        let sel = self.ts.tree_sel.as_deref();
        let Some(letter) = sel.and_then(|p| self.file_status(p)) else {
            return self.preview_view(cx);
        };
        let diff_mode = !self.ts.egit.file_mode;
        let seg = |id: &'static str, label: &'static str, on: bool, file_mode: bool| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(1.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(if on { strong() } else { dim() })
                .when(on, |d| d.bg(hsla(t::ELEVATED)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ts.egit.file_mode = file_mode;
                    (this.ts.sel, this.ts.menu) = (None, None);
                    cx.notify();
                }))
        };
        let strip = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(12.))
            .py(px(4.))
            .border_b_1()
            .border_color(hsla(t::RULE))
            .child(seg("fx-mode-file", "file", !diff_mode, true))
            .child(seg("fx-mode-diff", "diff", diff_mode, false))
            .child(
                div()
                    .ml_auto()
                    .text_color(hsla(badge_colour(letter)))
                    .child(letter.to_string()),
            )
            // the file view's header carries Refresh itself; the diff has none, so the strip does
            .when(diff_mode, |d| {
                d.child(
                    icon_btn(
                        "fx-refresh".into(),
                        "refresh",
                        "Refresh".into(),
                        dim(),
                        None,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_tree(cx))),
                )
            });
        let body = if !diff_mode {
            self.preview_view(cx)
        } else {
            match self
                .ts
                .egit
                .diff
                .as_ref()
                .filter(|(p, _)| Some(p.as_str()) == sel)
            {
                None => div().p(px(14.)).child(note("reading...")),
                Some((_, Err(why))) => div().p(px(14.)).child(note(why.clone())),
                Some((_, Ok(f))) if f.lines.is_empty() => {
                    div().p(px(14.)).child(note("no differences against HEAD"))
                }
                Some((_, Ok(f))) => div()
                    .py(px(6.))
                    .px(px(12.))
                    .text_size(px(super::filesel::FONT_PX))
                    .children(f.lines.iter().zip(sign_marks(&diff_signs(&f.lines))).map(
                        |(l, m)| {
                            // a pair of removed and added lines is an edit: blue bar on both
                            div()
                                .relative()
                                .children(m.map(|m| mark_bar(m).left(px(-10.))))
                                .child(git_line(l))
                        },
                    ))
                    .when(f.cut, |d| {
                        d.child(
                            div()
                                .text_color(with_alpha(dim(), 1.0))
                                .child("\u{2026} diff cut"),
                        )
                    }),
            }
        };
        div().flex().flex_col().child(strip).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_view::project::{ProjectData, TreeRow};

    fn row(depth: u16, name: &str, dir: bool, status: Option<char>) -> TreeRow {
        TreeRow {
            depth,
            name: name.into(),
            dir,
            status,
            ..Default::default()
        }
    }

    #[test]
    fn badge_colours_follow_the_palette() {
        for (c, want) in [
            ('M', t::WARN),
            ('R', t::WARN),
            ('A', t::SUCCESS),
            ('?', t::SUCCESS),
            ('D', t::ERROR),
            ('U', t::ERROR),
        ] {
            assert_eq!(badge_colour(c), want, "{c}");
        }
    }

    #[test]
    fn a_folder_shows_the_strongest_change_inside_it_ignored_files_do_not_count() {
        let mut d = ProjectData::default();
        d.listing.status.insert("a/b/new.txt".into(), '?');
        d.listing.status.insert("a/b/mod.rs".into(), 'M');
        d.listing.status.insert("a/c/x".into(), 'A');
        d.listing.status.insert("z/skip".into(), '!');
        d.listing.status.insert("top.txt".into(), 'M');
        let m = folder_marks(&d);
        assert_eq!(m.get("a/b"), Some(&'M'));
        assert_eq!(m.get("a"), Some(&'M'));
        assert_eq!(m.get("a/c"), Some(&'A'));
        assert_eq!(m.get("z"), None);
        assert_eq!(m.get("top.txt"), None);
        d.listing.status.insert("a/b/gone".into(), 'U');
        assert_eq!(folder_marks(&d).get("a"), Some(&'U'));
    }

    #[test]
    fn rows_alone_give_the_marks_too() {
        let d = ProjectData {
            rows: vec![
                row(0, "src", true, None),
                row(1, "a.rs", false, Some('D')),
                row(0, "docs", true, None),
            ],
            ..Default::default()
        };
        let m = folder_marks(&d);
        assert_eq!(m.get("src"), Some(&'D'));
        assert_eq!(m.get("docs"), None);
    }
}
