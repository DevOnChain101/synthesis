//! The centre panel's tabs next to the chat (`cli.html` `.bottomtabs`): diff, terminal, browser and file
//! explorer, on the live session. All read-only, none is a terminal and none writes anything.
//!
//! - diff: the files this session changed with `+N -N`, each opening to its syntax-coloured diff;
//! - terminal: the command history (from `process.*` / `tool.*` events): command, exit code, duration, output
//!   tail, running state;
//! - browser: the page, the saved `browser.frame` screenshots (latest and history) and the browser calls;
//! - file explorer: the project tree (git's file list in a trusted folder, a plain walk without git otherwise, directories open on demand, changed files
//!   marked) and a read-only text preview of the selected file (size and line caps, no secret files).
//!
//! What is open or selected lives in a [`TabState`] kept per session, so switching sessions and back finds the
//! tabs as they were.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use gpui::{
    Context, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::project::{self, ProjectData};

use super::panels::{OnClick, dim, icon, strong, text, with_alpha};
use super::{ChatTab, TuiView};
use crate::live::NoticeKind;
use crate::theme::hsla;
use crate::widgets::panes::{self, note};

/// Bytes of a file the preview reads at most, and the lines it draws.
pub const PREVIEW_BYTES: usize = 256 * 1024;
pub const PREVIEW_LINES: usize = 2000;

/// A file read for the preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub path: String,
    pub body: Result<PreviewText, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewText {
    pub lines: Vec<String>,
    /// Size of the file on disk.
    pub size: u64,
    /// Only the start is shown (the byte or line cap).
    pub cut: bool,
}

/// Reads `rel` (a path of the project tree) under `root` for the preview. Refuses what must not be shown: a
/// path that leaves the folder (`..`, an absolute path, a link out of it), a secret file, a file that is not
/// regular, binary content; reads at most [`PREVIEW_BYTES`] and shows at most [`PREVIEW_LINES`] lines.
pub fn read_preview(root: &Path, rel: &str) -> Result<PreviewText, String> {
    let p = Path::new(rel);
    if p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("not a path inside the project".into());
    }
    let full = root.join(p);
    // a link that leaves the folder is not followed
    let (real_root, real) = (
        std::fs::canonicalize(root).map_err(|e| e.to_string())?,
        std::fs::canonicalize(&full).map_err(|e| e.to_string())?,
    );
    if !real.starts_with(&real_root) {
        return Err("the file is outside the project folder".into());
    }
    if synthesis_core::secrets::is_secret_path(&real) || synthesis_core::secrets::is_secret_path(p)
    {
        return Err("a secret file (keys, .env, credentials) is not shown".into());
    }
    let meta = std::fs::metadata(&real).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut buf = Vec::new();
    std::fs::File::open(&real)
        .and_then(|f| f.take(PREVIEW_BYTES as u64 + 1).read_to_end(&mut buf))
        .map_err(|e| e.to_string())?;
    let cut_bytes = buf.len() > PREVIEW_BYTES;
    buf.truncate(PREVIEW_BYTES);
    if buf[..buf.len().min(8192)].contains(&0) {
        return Err(format!("binary file ({} bytes)", meta.len()));
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text
        .lines()
        .map(synthesis_harness::sanitize_for_display)
        .collect();
    let cut_lines = lines.len() > PREVIEW_LINES;
    lines.truncate(PREVIEW_LINES);
    Ok(PreviewText {
        lines,
        size: meta.len(),
        cut: cut_bytes || cut_lines,
    })
}

/// What the tabs keep for one session.
#[derive(Default)]
pub struct TabState {
    pub tab: ChatTab,
    /// Changed files whose diff is expanded; the newest file opens once by itself.
    pub diff_open: HashSet<String>,
    pub diff_init: bool,
    /// The saved browser frame shown (`None`: the latest).
    pub frame: Option<usize>,
    /// Commands (by id) of the terminal tab shown whole instead of their last 5 lines.
    pub term_open: HashSet<String>,
    /// Background processes the person stopped with Kill.
    pub killed: HashSet<String>,
    /// What the terminal tab showed last (commands, output lines, running): a change scrolls to the newest.
    pub term_sig: (usize, usize, usize),
    /// Directories of the file explorer that are open; changed files' parents open by themselves once.
    pub tree_open: HashSet<String>,
    pub tree_init: bool,
    pub tree_sel: Option<String>,
    pub preview: Option<Preview>,
    /// The transcript of this sub-agent shows beside the chat (`AgentPanes::subs` id).
    pub sub: Option<String>,
    /// Lines (0-based, anchor and head) selected in the preview; a drag is in progress; the right-click menu's spot.
    pub sel: Option<(usize, usize)>,
    pub sel_drag: bool,
    pub menu: Option<gpui::Point<gpui::Pixels>>,
    /// Quotes waiting for the next prompt (shown as chips under the input).
    pub quotes: Vec<super::quote::Quote>,
}

/// The project tree of the session's folder.
#[derive(Default)]
pub struct Tree {
    pub data: Option<ProjectData>,
    pub for_cwd: String,
    pub loading: bool,
    /// Changed files at the time of the scan: a different count means the tree is stale.
    pub at_changes: usize,
    /// Whether the folder was trusted at the scan (git) or not (plain walk).
    pub trusted: bool,
}

/// Paths of the tree rows (`dir/sub/file`), in row order.
pub fn row_paths(d: &ProjectData) -> Vec<String> {
    let mut stack: Vec<String> = vec![];
    d.rows
        .iter()
        .map(|r| {
            stack.truncate(usize::from(r.depth));
            let path = stack
                .iter()
                .map(String::as_str)
                .chain([r.name.as_str()])
                .collect::<Vec<_>>()
                .join("/");
            if r.dir {
                stack.push(r.name.clone());
            }
            path
        })
        .collect()
}

/// The rows to draw: a row under a closed directory is not built (the tree is "lazy": only what is open).
pub fn visible_rows(d: &ProjectData, open: &HashSet<String>) -> Vec<usize> {
    let paths = row_paths(d);
    let mut out = vec![];
    let mut hide_below: Option<u16> = None;
    for (i, r) in d.rows.iter().enumerate() {
        if let Some(depth) = hide_below {
            if r.depth > depth {
                continue;
            }
            hide_below = None;
        }
        out.push(i);
        if r.dir && !open.contains(&paths[i]) {
            hide_below = Some(r.depth);
        }
    }
    out
}

/// The directories to open at first: the parents of every changed file.
pub fn dirs_of_changes(d: &ProjectData) -> HashSet<String> {
    let paths = row_paths(d);
    let mut out = HashSet::new();
    for (i, r) in d.rows.iter().enumerate() {
        if !r.dir && r.status.is_some() {
            let mut p = Path::new(&paths[i]).parent();
            while let Some(q) = p.filter(|q| !q.as_os_str().is_empty()) {
                out.insert(q.to_string_lossy().into_owned());
                p = q.parent();
            }
        }
    }
    out
}

impl TuiView {
    /// A click or key on a tab. Files and Diff are for the person's own eyes (the agent never reads them), so
    /// they need no trust; the Trust dialog is `/trust ask`.
    pub(super) fn select_tab(&mut self, tab: ChatTab, cx: &mut Context<Self>) {
        self.ts.tab = tab;
        cx.notify();
    }

    /// Sets the tab and, when asked, what it shows (`/diff FILE`).
    pub(super) fn show_tab(&mut self, tab: ChatTab, file: Option<String>, cx: &mut Context<Self>) {
        self.select_tab(tab, cx);
        if let (ChatTab::Diff, Some(f)) = (tab, file) {
            let found = self.live.as_ref().and_then(|l| {
                l.live
                    .view
                    .panes
                    .files
                    .iter()
                    .find(|c| c.path == f || c.path.ends_with(&format!("/{f}")))
                    .map(|c| c.path.clone())
            });
            if let Some(p) = found {
                self.ts.diff_open.insert(p);
            }
            self.ts.diff_init = true;
        }
        cx.notify();
    }

    /// Per frame: the first look at a tab does its one-time setup (the newest changed file opens, the tree is
    /// read, its changed directories open). Cheap when nothing is due.
    pub(super) fn tabs_tick(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        let changes = lu.live.view.panes.files.len();
        if self.ts.tab == ChatTab::Diff
            && !self.ts.diff_init
            && let Some(f) = lu.live.view.panes.files.last()
        {
            let p = f.path.clone();
            self.ts.diff_open.insert(p);
            self.ts.diff_init = true;
        }
        if self.ts.tab == ChatTab::Terminal {
            let c = &lu.live.view.panes.commands;
            let sig = (
                c.len(),
                c.iter().map(|c| c.tail.len()).sum(),
                c.iter()
                    .filter(|c| c.state == synthesis_view::agent::CmdState::Running)
                    .count(),
            );
            if sig != self.ts.term_sig {
                self.ts.term_sig = sig;
                self.pane_scroll.scroll_to_bottom();
            }
        }
        if self.ts.tab == ChatTab::Files {
            // the plain listing of an untrusted folder gives way to the git one once it is trusted
            let stale = self.tree.for_cwd != lu.live.cwd
                || self.tree.at_changes != changes
                || self.tree.trusted != lu.live.trusted;
            if (self.tree.data.is_none() || stale) && !self.tree.loading {
                self.request_scan(cx);
            }
        }
    }

    /// The Browser tab shows the agent cursor and motion is on: the ring pulses and the cursor fades, so the
    /// window repaints every frame meanwhile.
    pub(super) fn pointer_animating(&self) -> bool {
        let Some(lu) = &self.live else { return false };
        self.ts.tab == ChatTab::Browser
            && lu.cfg.tui.motion
            && lu.live.view.panes.pointer.as_ref().is_some_and(|p| {
                p.ts.is_some_and(|ts| {
                    crate::live::now_ms() - ts < panes::POINTER_HOLD_MS + panes::POINTER_FADE_MS
                })
            })
    }

    /// The chat scrolls to the tool row `call_id` (the diff tab's "chat" button). Nothing if the row is gone.
    fn jump_to_tool(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        let at = lu.live.view.convo.items.iter().position(
            |i| matches!(i, synthesis_view::convo::Item::Tool { id, .. } if id == call_id),
        );
        match at {
            Some(anchor) => self.jump_to_item(anchor, cx),
            None => {
                self.ts.tab = ChatTab::Chat;
                cx.notify();
            }
        }
    }

    /// The Kill button of a running background process: the person's own stop of a process of THIS session
    /// (the terminal runtime refuses any other id); it is not a model tool call and needs no approval.
    fn kill_process(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        match e.kill_process(id) {
            Ok(true) => {
                self.ts.killed.insert(id.to_string());
                lu.live.note_kind(
                    NoticeKind::Bookkeeping,
                    format!("process {id} stopped by you"),
                );
            }
            Ok(false) => lu
                .live
                .note(format!("{id} had already exited: nothing was sent"), false),
            Err(why) => lu.live.note(format!("could not stop {id}: {why}"), true),
        }
        lu.dirty = true;
        cx.notify();
    }

    /// Reads the project tree off the UI thread: git (read-only) in a trusted folder, a plain walk without git
    /// in an untrusted one.
    pub fn request_scan(&mut self, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        let cwd = lu.live.cwd.clone();
        let changes = lu.live.view.panes.files.len();
        if cwd.is_empty() {
            return;
        }
        self.tree.loading = true;
        let (dir, env, trusted) = (PathBuf::from(&cwd), lu.cfg.env.clone(), lu.live.trusted);
        // WSL mode: the engine scans its own folder (a path of this machine would be the Windows view of it)
        let remote = lu.live.engine.clone().filter(|e| e.is_remote());
        let job = cx.background_spawn(async move {
            if let Some(e) = remote {
                return e
                    .remote()
                    .ok_or_else(|| "not a WSL engine".to_string())
                    .and_then(|r| r.fs_list())
                    .unwrap_or_else(|why| ProjectData {
                        loaded: true,
                        note: Some(why),
                        ..Default::default()
                    });
            }
            if trusted {
                project::scan(&dir, &env)
            } else {
                project::scan_plain(&dir)
            }
        });
        cx.spawn(async move |this, cx| {
            let d = job.await;
            this.update(cx, |t, cx| {
                t.tree.loading = false;
                if !t.ts.tree_init {
                    t.ts.tree_open = dirs_of_changes(&d);
                    t.ts.tree_init = true;
                }
                t.tree.data = Some(d);
                t.tree.for_cwd = cwd;
                t.tree.at_changes = changes;
                t.tree.trusted = trusted;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A click on a file of the tree: select it and read it off the UI thread.
    fn open_preview(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(lu) = &self.live else { return };
        let root = PathBuf::from(&lu.live.cwd);
        self.ts.tree_sel = Some(path.clone());
        self.ts.preview = None;
        (self.ts.sel, self.ts.menu) = (None, None);
        let rel = path.clone();
        let remote = lu.live.engine.clone().filter(|e| e.is_remote());
        let job = cx.background_spawn(async move {
            match remote {
                Some(e) => e
                    .remote()
                    .ok_or_else(|| "not a WSL engine".to_string())
                    .and_then(|r| r.fs_read(&rel))
                    .map(|(lines, size, cut)| PreviewText { lines, size, cut }),
                None => read_preview(&root, &rel),
            }
        });
        cx.spawn(async move |this, cx| {
            let body = job.await;
            this.update(cx, |t, cx| {
                if t.ts.tree_sel.as_deref() == Some(path.as_str()) {
                    t.ts.preview = Some(Preview { path, body });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Up/Down in the file explorer: the next visible row; a file opens its preview, a directory is only marked.
    pub(super) fn step_tree(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let Some(d) = &self.tree.data else {
            return false;
        };
        let paths = row_paths(d);
        let vis = visible_rows(d, &self.ts.tree_open);
        let at = vis
            .iter()
            .position(|i| self.ts.tree_sel.as_deref() == Some(paths[*i].as_str()));
        let next = match (at, down) {
            (None, _) => vis.first(),
            (Some(a), true) => vis.get(a + 1),
            (Some(a), false) => a.checked_sub(1).and_then(|a| vis.get(a)),
        };
        let Some(&i) = next else { return true };
        let path = paths[i].clone();
        if d.rows[i].dir {
            self.ts.tree_sel = Some(path);
            cx.notify();
        } else {
            self.open_preview(path, cx);
        }
        true
    }

    /// Whether the file explorer has read the tree (the offscreen shots wait for it).
    pub fn tree_loaded(&self) -> bool {
        self.tree.data.is_some() && !self.tree.loading
    }

    /// Selects `path` in the file explorer and reads it for the preview.
    pub fn select_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.open_preview(path.to_string(), cx);
    }

    /// The counters of the tab strip: changed files, commands, browser calls.
    pub(super) fn badge(&self, tab: ChatTab) -> Option<String> {
        let p = &self.live.as_ref()?.live.view.panes;
        let n = match tab {
            ChatTab::Diff => p.files.len(),
            ChatTab::Terminal => p.commands.len(),
            ChatTab::Browser => p.browser.len(),
            _ => 0,
        };
        (n > 0).then(|| n.to_string())
    }

    /// The body of a tab other than chat.
    pub(super) fn pane(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty;
        let live = match &self.live {
            Some(l) => &l.live,
            None => {
                empty = crate::live::Live::new();
                &empty
            }
        };
        let tz = self.live.as_ref().map_or(0, |l| l.cfg.tz_min);
        let inner = match self.ts.tab {
            ChatTab::Chat => div(),
            ChatTab::Diff => {
                let toggle = |path: String| -> OnClick {
                    Box::new(cx.listener(move |this, _, _, cx| {
                        if !this.ts.diff_open.remove(&path) {
                            this.ts.diff_open.insert(path.clone());
                        }
                        cx.notify();
                    }))
                };
                let jump = |call: String| -> OnClick {
                    Box::new(cx.listener(move |this, _, _, cx| this.jump_to_tool(&call, cx)))
                };
                panes::diff(live, &self.ts.diff_open, &toggle, &jump)
            }
            ChatTab::Terminal => {
                let toggle = |id: String| -> OnClick {
                    Box::new(cx.listener(move |this, _, _, cx| {
                        if !this.ts.term_open.remove(&id) {
                            this.ts.term_open.insert(id.clone());
                        }
                        cx.notify();
                    }))
                };
                let kill = |id: String| -> OnClick {
                    Box::new(cx.listener(move |this, _, _, cx| this.kill_process(&id, cx)))
                };
                panes::commands(
                    live,
                    &panes::TermView {
                        open: &self.ts.term_open,
                        killed: &self.ts.killed,
                        toggle: &toggle,
                        kill: &kill,
                    },
                )
            }
            ChatTab::Browser => {
                let home = self.live.as_ref().map(|l| l.cfg.synthesis_home.clone());
                let pick = |i: usize| -> OnClick {
                    Box::new(cx.listener(move |this, _, _, cx| {
                        this.ts.frame = Some(i);
                        cx.notify();
                    }))
                };
                let motion = self.live.as_ref().is_none_or(|l| l.cfg.tui.motion);
                panes::browser(
                    live,
                    self.ts.frame,
                    tz,
                    home.as_deref(),
                    &pick,
                    crate::live::now_ms(),
                    motion,
                )
            }
            ChatTab::Files => return self.files_tab(cx),
        };
        div()
            .id(("pane", self.ts.tab as usize))
            .debug_selector(|| "tab-pane".to_string())
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.pane_scroll)
            .py(px(10.))
            .child(inner)
            .into_any_element()
    }

    /// `.fx`: the tree on the left (250px), the preview of the selected file on the right.
    fn files_tab(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let shell = |body: Div| {
            div()
                .debug_selector(|| "tab-pane".to_string())
                .flex_1()
                .min_h(px(0.))
                .my(px(10.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .rounded(px(8.))
                .overflow_hidden()
                .child(body)
                .into_any_element()
        };
        let Some(d) = self.tree.data.as_ref() else {
            return shell(div().p(px(14.)).child(note(if self.tree.loading {
                "reading the project..."
            } else {
                "The project tree loads when this tab opens."
            })));
        };
        if let Some(n) = &d.note {
            return shell(div().p(px(14.)).child(note(n.clone())));
        }
        let paths = row_paths(d);
        let vis = visible_rows(d, &self.ts.tree_open);
        let mut rows = div().flex().flex_col().py(px(4.)).px(px(8.));
        for i in vis {
            let r = &d.rows[i];
            let path = paths[i].clone();
            let sel = self.ts.tree_sel.as_deref() == Some(path.as_str());
            let open = self.ts.tree_open.contains(&path);
            let st = r.status.map(|c| {
                (
                    c.to_string(),
                    hsla(match c {
                        'A' | '?' => t::SUCCESS,
                        'D' => t::ERROR,
                        _ => t::WARN,
                    }),
                )
            });
            let click_path = path.clone();
            let dir = r.dir;
            rows = rows.child(
                div()
                    .id(("tree-row", i))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(21.))
                    .pl(px(f32::from(r.depth) * 16.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_color(if sel { strong() } else { text() })
                    .when(sel, |d| d.bg(hsla(t::ELEVATED)))
                    .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.05)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if dir {
                            if !this.ts.tree_open.remove(&click_path) {
                                this.ts.tree_open.insert(click_path.clone());
                            }
                            cx.notify();
                        } else {
                            this.open_preview(click_path.clone(), cx);
                        }
                    }))
                    .child(
                        div()
                            .w(px(10.))
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(dim())
                            .child(if !dir {
                                ""
                            } else if open {
                                "\u{25BE}"
                            } else {
                                "\u{25B8}"
                            }),
                    )
                    .child(icon(
                        if dir { "folder" } else { "file" },
                        14.,
                        if dir { hsla(t::INFO) } else { dim() },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(if dir {
                                hsla(t::INFO)
                            } else if sel {
                                strong()
                            } else {
                                text()
                            })
                            .child(synthesis_harness::sanitize_for_display(&r.name)),
                    )
                    .children(st.map(|(l, c)| {
                        div()
                            .pr(px(6.))
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(c)
                            .child(l)
                    })),
            );
        }
        if d.rows.is_empty() {
            rows = rows.child(note(
                "No files to list yet: the project has no tracked or new files.",
            ));
        }
        if d.truncated {
            rows = rows.child(
                div()
                    .px(px(6.))
                    .text_color(dim())
                    .child("\u{2026} cut at the display limit"),
            );
        }
        let tree = div()
            .id("fx-tree")
            .w(px(250.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(hsla(t::RULE))
            .child(rows);
        let body = div()
            .id("fx-body")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .overflow_scroll()
            .child(self.preview_view(cx));
        shell(div().size_full().flex().child(tree).child(body))
    }

    fn preview_view(&self, cx: &mut Context<Self>) -> Div {
        let Some(sel) = &self.ts.tree_sel else {
            return div()
                .p(px(14.))
                .child(note("Select a file to preview it (read-only)."));
        };
        let Some(p) = self.ts.preview.as_ref().filter(|p| &p.path == sel) else {
            return div().p(px(14.)).child(note("reading..."));
        };
        let head = |right: String| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(7.))
                .border_b_1()
                .border_color(hsla(t::RULE))
                .child(icon("file", 14., dim()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(strong())
                        .child(p.path.clone()),
                )
                .child(div().flex_none().text_color(dim()).child(right))
        };
        let copy = div()
            .id("copy-file")
            .flex_none()
            .px(px(6.))
            .rounded(px(4.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .text_color(strong())
            .cursor_pointer()
            .child("copy")
            .on_click(cx.listener(|this, _, _, cx| {
                this.ts.sel = None;
                this.copy_selection(cx)
            }));
        match &p.body {
            Err(why) => div()
                .child(head(String::new()))
                .child(div().p(px(14.)).child(note(why.clone()))),
            Ok(b) => {
                let n = b.lines.len();
                let w = format!("{}", n).len().max(3) as f32 * 7.5 + 12.;
                div()
                    .flex()
                    .flex_col()
                    .child(head(format!("{n} lines \u{b7} {}", human_size(b.size))).child(copy))
                    .child(
                        div()
                            .py(px(6.))
                            .children(b.lines.iter().enumerate().map(|(i, l)| {
                                let on = self
                                    .ts
                                    .sel
                                    .is_some_and(|(a, z)| (a.min(z)..=a.max(z)).contains(&i));
                                div()
                                    .flex()
                                    .px(px(12.))
                                    .whitespace_nowrap()
                                    .cursor_text()
                                    .hover(|d| d.bg(with_alpha(hsla(t::TEXT_STRONG), 0.03)))
                                    .when(on, |d| d.bg(with_alpha(hsla(t::ACCENT), 0.22)))
                                    // line-wise selection: press, drag over lines, release; right-click opens the menu
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(
                                            move |this, e: &gpui::MouseDownEvent, _, cx| {
                                                let from = this
                                                    .ts
                                                    .sel
                                                    .filter(|_| e.modifiers.shift)
                                                    .map_or(i, |s| s.0);
                                                this.ts.sel = Some((from, i));
                                                (this.ts.sel_drag, this.ts.menu) = (true, None);
                                                cx.notify();
                                            },
                                        ),
                                    )
                                    .on_mouse_move(cx.listener(
                                        move |this, e: &gpui::MouseMoveEvent, _, cx| {
                                            if !this.ts.sel_drag {
                                                return;
                                            }
                                            if e.pressed_button != Some(gpui::MouseButton::Left) {
                                                this.ts.sel_drag = false;
                                            } else if let Some(s) = &mut this.ts.sel
                                                && s.1 != i
                                            {
                                                s.1 = i;
                                                cx.notify();
                                            }
                                        },
                                    ))
                                    .on_mouse_up(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _, _, _| this.ts.sel_drag = false),
                                    )
                                    .on_mouse_down(
                                        gpui::MouseButton::Right,
                                        cx.listener(
                                            move |this, e: &gpui::MouseDownEvent, _, cx| {
                                                if !this.ts.sel.is_some_and(|(a, z)| {
                                                    (a.min(z)..=a.max(z)).contains(&i)
                                                }) {
                                                    this.ts.sel = Some((i, i));
                                                }
                                                (this.ts.sel_drag, this.ts.menu) =
                                                    (false, Some(e.position));
                                                cx.notify();
                                            },
                                        ),
                                    )
                                    .child(
                                        div()
                                            .w(px(w))
                                            .flex_none()
                                            .pr(px(12.))
                                            .text_right()
                                            .text_color(dim())
                                            .child((i + 1).to_string()),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.))
                                            .flex_1()
                                            .overflow_hidden()
                                            .text_color(text())
                                            .child(crate::widgets::syntax::styled(
                                                if l.is_empty() { " " } else { l },
                                            )),
                                    )
                            })),
                    )
                    .when(b.cut, |d| {
                        d.child(
                            div()
                                .px(px(12.))
                                .pb(px(8.))
                                .text_color(dim())
                                .child(format!(
                                    "\u{2026} only the start is shown (first {} KB / {} lines)",
                                    PREVIEW_BYTES / 1024,
                                    PREVIEW_LINES
                                )),
                        )
                    })
            }
        }
    }
}

fn human_size(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_view::project::TreeRow;

    fn row(depth: u16, name: &str, dir: bool, status: Option<char>) -> TreeRow {
        TreeRow {
            depth,
            name: name.into(),
            dir,
            status,
        }
    }

    fn data() -> ProjectData {
        ProjectData {
            loaded: true,
            rows: vec![
                row(0, "src", true, None),
                row(1, "lib", true, None),
                row(2, "a.rs", false, Some('M')),
                row(1, "main.rs", false, None),
                row(0, "tests", true, None),
                row(1, "t.rs", false, None),
                row(0, "README.md", false, None),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn row_paths_follow_the_depth() {
        let p = row_paths(&data());
        assert_eq!(
            p,
            [
                "src",
                "src/lib",
                "src/lib/a.rs",
                "src/main.rs",
                "tests",
                "tests/t.rs",
                "README.md"
            ]
        );
    }

    #[test]
    fn only_open_directories_show_their_rows() {
        let d = data();
        assert_eq!(visible_rows(&d, &HashSet::new()), [0, 4, 6]);
        let open: HashSet<String> = ["src".to_string()].into();
        assert_eq!(visible_rows(&d, &open), [0, 1, 3, 4, 6]);
        let open: HashSet<String> = ["src".to_string(), "src/lib".to_string()].into();
        assert_eq!(visible_rows(&d, &open), [0, 1, 2, 3, 4, 6]);
    }

    #[test]
    fn changed_files_open_their_directories() {
        let o = dirs_of_changes(&data());
        assert_eq!(o, ["src".to_string(), "src/lib".to_string()].into());
    }

    #[test]
    fn the_preview_refuses_what_it_must_and_caps_what_it_shows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(root.join("big.txt"), "x\n".repeat(PREVIEW_LINES + 10)).unwrap();
        let ok = read_preview(root, "a.txt").unwrap();
        assert_eq!((ok.lines.len(), ok.cut), (2, false));
        assert!(read_preview(root, ".env").unwrap_err().contains("secret"));
        assert!(
            read_preview(root, "bin.dat")
                .unwrap_err()
                .contains("binary")
        );
        assert!(read_preview(root, "../a.txt").is_err());
        assert!(read_preview(root, "/etc/hosts").is_err());
        let big = read_preview(root, "big.txt").unwrap();
        assert_eq!((big.lines.len(), big.cut), (PREVIEW_LINES, true));
        #[cfg(unix)]
        {
            let out = tempfile::tempdir().unwrap();
            std::fs::write(out.path().join("o.txt"), "outside").unwrap();
            std::os::unix::fs::symlink(out.path().join("o.txt"), root.join("link.txt")).unwrap();
            assert!(
                read_preview(root, "link.txt")
                    .unwrap_err()
                    .contains("outside")
            );
        }
    }
}
