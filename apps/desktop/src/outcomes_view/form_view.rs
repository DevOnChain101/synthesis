//! Drawing of the "New outcome" / "Revise contract" form (`form.rs` is its state). A modal panel over the
//! tab: fields with their hints, the errors inline at the top, the buttons at the bottom; then, after the
//! server stored the contract, the review (the server's diff, or the contract itself for a first version)
//! with the one explicit "Approve" button. Palette: platinum with the accent, no blue or violet.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use gpui_component::input::{Input, Textarea};
use synthesis_cli_ui_render::theme as t;

use super::OutcomesView;
use super::form::{DiffView, Form, Head, Mode, Review, Stage};
use super::render::{contract_block, heading, mono};
use crate::theme::{self, hsla};
use crate::widgets::chrome::{black_a, dim, rule, strong, text};

/// A small text button of the tab (the list's "New outcome", the Goal tab's actions).
pub(super) fn small_button(
    id: &'static str,
    label: &str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .cursor_pointer()
        .flex_none()
        .px(px(9.))
        .py(px(3.))
        .rounded(px(6.))
        .border_1()
        .border_color(hsla(t::ACCENT))
        .text_color(hsla(t::ACCENT_LIGHT))
        .hover(|d| d.bg(hsla(t::ELEVATED)))
        .on_click(on_click)
        .child(label.to_string())
}

fn label(name: &str, hint: &str) -> Div {
    div()
        .mt(px(10.))
        .mb(px(3.))
        .flex()
        .items_baseline()
        .gap(px(8.))
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(strong())
                .child(name.to_string()),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(dim())
                .child(hint.to_string()),
        )
}

/// What the buttons say, for the stage.
fn button_labels(f: &Form) -> (&'static str, &'static str) {
    match (&f.stage, &f.mode) {
        (Stage::Edit, Mode::New) => ("Create outcome", "Cancel"),
        (Stage::Edit, Mode::Revise { .. }) => ("Submit revision", "Cancel"),
        (Stage::Sending, _) => ("Sending...", "Close"),
        (Stage::Review(_), _) => ("Approve contract", "Not now"),
        (Stage::Approving(_), _) => ("Approving...", "Close"),
        (Stage::Edit, Mode::Draft { .. }) => ("Try again", "Close"),
    }
}

fn diff_block(d: &DiffView) -> Div {
    let mut b = div().flex().flex_col().child(heading(&format!(
        "Changes against the approved contract (version {})",
        d.base_version
    )));
    if d.changes.is_empty() {
        b = b.child(div().text_color(dim()).child("No field differs."));
    }
    for c in &d.changes {
        b = b.child(
            div()
                .py(px(3.))
                .child(
                    mono(format!("{} {}", c.area, c.key))
                        .text_size(px(11.))
                        .text_color(dim()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(div().w(px(32.)).flex_none().text_color(dim()).child("was"))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(text())
                                .child(c.old.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(div().w(px(32.)).flex_none().text_color(dim()).child("now"))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(strong())
                                .child(c.new.clone()),
                        ),
                ),
        );
    }
    b
}

/// A warning the server derived (widens, weakens): its words, not ours, under a heading that says what it is.
fn warning(id: &'static str, head: &str, items: &[String]) -> Option<Div> {
    (!items.is_empty()).then(|| {
        div()
            .debug_selector(move || id.to_string())
            .mt(px(10.))
            .p(px(10.))
            .rounded(px(8.))
            .border_1()
            .border_color(hsla(t::WARN))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(hsla(t::WARN))
                    .child(head.to_string()),
            )
            .children(
                items
                    .iter()
                    .map(|i| div().text_color(text()).child(format!("- {i}"))),
            )
    })
}

fn review_body(f: &Form, r: &Review) -> Div {
    let first = r.version == 1;
    let lead = match (&f.mode, first) {
        (Mode::New, _) if first => {
            "The outcome is created. Its contract is NOT approved: nothing runs until you approve it."
        }
        (Mode::Draft { .. }, true) => {
            "This contract is NOT approved: nothing runs until you approve it."
        }
        _ => {
            "A new contract version is stored and not approved. The approved contract stays in force until you approve this one."
        }
    };
    let mut d = div()
        .flex()
        .flex_col()
        .child(div().text_color(text()).child(lead))
        .child(
            mono(format!("version {} \u{b7} hash {}", r.version, r.hash))
                .mt(px(6.))
                .text_size(px(11.))
                .text_color(dim()),
        );
    match &r.diff {
        None => {}
        Some(Ok(diff)) => {
            d = d.children(warning(
                "out-widens",
                "WARNING: this revision gives attempts MORE room than the approved contract",
                &diff.widens,
            ));
            d = d.children(warning(
                "out-weakens",
                "WARNING: this revision lowers the bar for done",
                &diff.weakens,
            ));
            d = d.child(diff_block(diff));
        }
        Some(Err(e)) => {
            d = d.child(div().mt(px(10.)).text_color(hsla(t::WARN)).child(format!(
                "The server's diff cannot be shown, so this contract cannot be approved here: {e}"
            )));
        }
    }
    match &r.contract {
        Ok(c) => d = d.child(contract_block(c)),
        Err(e) => {
            d = d.child(div().mt(px(10.)).text_color(hsla(t::WARN)).child(format!(
                "The stored contract cannot be shown, so it cannot be approved here: {e}"
            )));
        }
    }
    d.child(
        div()
            .mt(px(10.))
            .text_size(px(11.))
            .text_color(dim())
            .child(format!(
                "Approve sends version {} with the hash above. Nothing is approved until you press the button (Tab to it, then Enter).",
                r.version
            )),
    )
}

impl OutcomesView {
    fn fields(&self, f: &Form, window: &Window, cx: &Context<Self>) -> Option<Div> {
        let ui = self.form_ui.as_ref()?;
        let mut d = div()
            .flex()
            .flex_col()
            .child(label("Goal", "what the outcome must achieve"))
            .child(Textarea::new(&ui.goal))
            .child(label("Acceptance criteria", "one per line; each must be met"))
            .child(Textarea::new(&ui.criteria))
            .child(label(
                "Checks",
                "verification commands, one per line; they are run independently to verify the outcome",
            ))
            .child(Textarea::new(&ui.checks))
            .child(label("Budget (USD)", "the most you accept to spend; no default"))
            .child(Input::new(&ui.budget))
            .child(label(
                "Write scope",
                "path patterns attempts may change, one per line (src/** for a folder)",
            ))
            .child(Textarea::new(&ui.scope));
        if f.is_new() {
            let (base, warn) = match &f.head {
                None => (
                    "Enter the workspace folder: its HEAD is read here.".to_string(),
                    false,
                ),
                Some(Head::Resolving(_)) => ("Reading HEAD...".to_string(), false),
                Some(Head::Known { commit, .. }) => (commit.clone(), false),
                Some(Head::Failed { why, .. }) => (why.clone(), true),
            };
            d = d
                .child(label(
                    "Workspace",
                    "the repository's folder (absolute path)",
                ))
                .child(self.workspace_picker(ui, window, cx))
                .child(label(
                    "Base commit",
                    "HEAD of the workspace; the work starts from this commit",
                ))
                .child(
                    mono(base)
                        .debug_selector(|| "out-form-base".to_string())
                        .text_color(if warn { hsla(t::WARN) } else { text() }),
                );
        } else {
            d = d.child(
                div()
                    .mt(px(10.))
                    .text_size(px(11.))
                    .text_color(dim())
                    .child("A contract can be revised while the outcome is blocked with no live attempt; the orchestrator decides. What this form does not show (denied paths, capabilities, per-attempt limits) stays as it is."),
            );
        }
        let _ = cx;
        Some(d)
    }

    /// The workspace field: a button showing the chosen folder (read-only) that opens the list of known folders and
    /// "Choose folder..."; for a remote engine the path is typed (a local dialog cannot name its folders).
    fn workspace_picker(&self, ui: &super::FormUi, window: &Window, cx: &Context<Self>) -> Div {
        let st = &self.state;
        let chosen = ui.workspace.read(cx).value().to_string();
        let chosen = chosen.trim();
        let focused = ui.ws_focus.is_focused(window);
        let shown = if chosen.is_empty() {
            "No folder chosen".to_string()
        } else {
            middle(chosen, 80)
        };
        let button = div()
            .id("out-form-ws")
            .debug_selector(|| "out-form-ws".to_string())
            .track_focus(&ui.ws_focus)
            .cursor_pointer()
            .mt(px(4.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(6.))
            .border_1()
            .border_color(hsla(if focused { t::ACCENT_LIGHT } else { t::BORDER }))
            .when(focused, |d| d.border_2())
            .flex()
            .flex_col()
            .on_click(self.click(cx, |this, cx| {
                this.state.ws_menu = if this.state.ws_menu.is_some() {
                    None
                } else {
                    Some(0)
                };
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(if st.remote {
                        "Known folders...".to_string()
                    } else {
                        shown.clone()
                    })
                    .child(div().text_color(dim()).child(if st.ws_menu.is_some() {
                        "\u{25b4}"
                    } else {
                        "\u{25be}"
                    })),
            )
            .when(!st.remote && !chosen.is_empty() && shown != chosen, |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(dim())
                        .child(chosen.to_string()),
                )
            })
            .when(
                !st.remote && chosen == st.workspace.trim() && !chosen.is_empty(),
                |d| {
                    d.child(
                        div()
                            .text_size(px(11.))
                            .text_color(dim())
                            .child("The app's workspace, used unless you choose another."),
                    )
                },
            );
        let items = st.ws_items();
        let list = st.ws_menu.map(|cur| {
            let mut col = div()
                .id("out-form-ws-list")
                .debug_selector(|| "out-form-ws-list".to_string())
                .mt(px(2.))
                .flex()
                .flex_col()
                .rounded(px(6.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .bg(theme::fill(t::ELEVATED));
            let row =
                |i: usize, id: &'static str| {
                    div()
                        .id((id, i))
                        .cursor_pointer()
                        .px(px(9.))
                        .py(px(4.))
                        .when(i == cur, |d| {
                            d.bg(hsla(t::ACCENT)).text_color(hsla(t::ACCENT_INK))
                        })
                        .hover(|d| d.bg(hsla(t::ACCENT)))
                        .on_click(self.click_window(cx, move |this, window, cx| {
                            this.ws_choose(i, window, cx)
                        }))
                };
            for (i, p) in items.iter().enumerate() {
                let (name, parent) = split_folder(p);
                col = col.child(
                    row(i, "out-form-ws-item")
                        .flex()
                        .gap(px(8.))
                        .child(name)
                        .child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .when(i != cur, |d| d.text_color(dim()))
                                .child(parent),
                        ),
                );
            }
            if !st.remote {
                col = col.child(
                    row(items.len(), "out-form-ws-item")
                        .border_t_1()
                        .border_color(rule())
                        .child("Choose folder..."),
                );
            }
            col
        });
        let mut d = div().flex().flex_col().child(button).children(list);
        if st.remote {
            d = d
                .child(Input::new(&ui.workspace))
                .child(div().text_size(px(11.)).text_color(dim()).child(
                    "The engine runs on another machine: type the folder as that machine names it (a local folder dialog would not fit).",
                ));
        }
        d
    }

    /// The form as a modal over the tab; `None` when no form is open.
    pub(super) fn form_overlay(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let f = self.state.form.as_ref()?;
        let ui = self.form_ui.as_ref()?;
        let title = match &f.mode {
            Mode::New => "New outcome".to_string(),
            Mode::Revise { outcome, .. } => format!("Revise contract of {outcome}"),
            Mode::Draft { outcome } => format!("Approve the contract of {outcome}"),
        };
        let body = match (&f.stage, &f.mode) {
            (Stage::Review(r) | Stage::Approving(r), _) => review_body(f, r),
            (Stage::Sending, Mode::Draft { .. }) => div()
                .text_color(dim())
                .child("Reading the contract from the orchestrator..."),
            (_, Mode::Draft { .. }) => div(),
            _ => self.fields(f, window, cx)?,
        };
        let sending = matches!(f.stage, Stage::Sending | Stage::Approving(_));
        let can_confirm = match &f.stage {
            Stage::Edit => true,
            Stage::Review(r) => r.approvable(),
            _ => false,
        };
        let (yes, no) = button_labels(f);
        let ring = |focused: bool| {
            if focused {
                hsla(t::ACCENT_LIGHT)
            } else {
                hsla(t::ACCENT)
            }
        };
        let confirm = div()
            .id("out-form-confirm")
            .debug_selector(|| "out-form-confirm".to_string())
            .track_focus(&ui.confirm)
            .cursor_pointer()
            .flex_none()
            .px(px(14.))
            .py(px(6.))
            .rounded(px(8.))
            .border_1()
            .font_weight(FontWeight::MEDIUM)
            .when(can_confirm, |d| {
                d.bg(hsla(t::ACCENT))
                    .border_color(ring(ui.confirm.is_focused(window)))
                    .text_color(hsla(t::ACCENT_INK))
            })
            .when(!can_confirm, |d| {
                d.border_color(hsla(t::RULE)).text_color(dim())
            })
            .when(ui.confirm.is_focused(window), |d| d.border_2())
            .on_click(self.click(cx, |this, cx| this.confirm(cx)))
            .child(yes);
        let cancel = div()
            .id("out-form-cancel")
            .debug_selector(|| "out-form-cancel".to_string())
            .track_focus(&ui.cancel)
            .cursor_pointer()
            .flex_none()
            .px(px(14.))
            .py(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(if ui.cancel.is_focused(window) {
                hsla(t::ACCENT_LIGHT)
            } else {
                hsla(t::BORDER)
            })
            .text_color(text())
            .on_click(self.click(cx, |this, cx| this.close_form(cx)))
            .child(no);
        let errors = (!f.errors.is_empty()).then(|| {
            div()
                .debug_selector(|| "out-form-errors".to_string())
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(2.))
                .px(px(14.))
                .py(px(8.))
                .border_b_1()
                .border_color(rule())
                .text_color(hsla(t::WARN))
                .children(f.errors.iter().map(|e| div().child(e.clone())))
        });
        let panel = div()
            .debug_selector(|| "out-form".to_string())
            .w(px(720.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(10.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .bg(theme::fill(t::SURFACE))
            .child(
                div()
                    .flex_none()
                    .px(px(14.))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(rule())
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(strong())
                    .child(title),
            )
            .children(errors)
            .child(
                div()
                    .id("out-form-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.form_scroll)
                    .px(px(14.))
                    .pb(px(12.))
                    .child(body),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(rule())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(11.))
                            .text_color(dim())
                            .child(if sending {
                                "Waiting for the orchestrator. Esc closes this window; a request already sent is not recalled."
                            } else {
                                "Tab and Shift+Tab move between the fields and the buttons. Enter acts on a button only. Esc closes without sending."
                            }),
                    )
                    .child(cancel)
                    .child(confirm),
            );
        Some(
            div()
                .debug_selector(|| "out-form-scrim".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.))
                .bg(black_a(0.55))
                .child(panel)
                .into_any_element(),
        )
    }
}

/// `s` cut in the middle to `max` characters (the ends of a path are the part that tells folders apart).
pub(super) fn middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let (a, b) = ((max - 1) / 2, max - 1 - (max - 1) / 2);
    let head: String = s.chars().take(a).collect();
    let tail: String = s.chars().skip(n - b).collect();
    format!("{head}\u{2026}{tail}")
}

/// A folder's name and its parent path (either separator: a remote path may not be this machine's).
pub(super) fn split_folder(p: &str) -> (String, String) {
    let t = p.trim_end_matches(['/', '\\']);
    match t.rfind(['/', '\\']) {
        Some(i) if i + 1 < t.len() => (t[i + 1..].to_string(), t[..i].to_string()),
        _ => (t.to_string(), String::new()),
    }
}
