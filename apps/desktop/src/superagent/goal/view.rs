//! The form as the mock's `vNew` (a column 728 wide: title, one sentence, labelled fields on surface plates, then
//! Create draft and Cancel), with the fields of S1 section 1 for the two kinds. Tokens and kit only.

use gpui::Entity;
use gpui::{
    AnyElement, Context, Div, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};

use super::model::Field;
use super::{GoalForm, review};
use crate::superagent::frames::GoalKind;
use crate::superagent::kit::{self, ButtonKind, text};
use crate::superagent::outcomes::form::Stage;
use crate::superagent::outcomes::model::Screen;
use crate::superagent::tokens::{self as t, hsla};

/// What the folder field says about a read-only goal: the worker reads the whole folder.
pub const READ_ONLY_FOLDER_NOTE: &str = "The worker reads the whole folder and what it reads goes to your model provider. Secret files (keys, .env) are not readable by it, but pick a folder that holds nothing else you would not share.";

/// The mod key as the person's keyboard names it.
pub fn mod_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}

/// A plate (mock `.fld`): surface fill, 1 px border (accent while it holds the focus, beige when it has an error).
fn plate(id: &'static str, focused: bool, bad: bool, child: impl IntoElement) -> Div {
    div()
        .debug_selector(move || id.to_string())
        .px(px(t::S2))
        .rounded(px(t::R_CONTROL))
        .bg(hsla(t::SURFACE))
        .text_color(hsla(t::TEXT_STRONG))
        .border_1()
        .border_color(hsla(if bad {
            t::SOFT_BEIGE
        } else if focused {
            t::ACCENT
        } else {
            t::BORDER
        }))
        .child(child)
}

impl GoalForm {
    fn focused(&self, h: gpui::FocusHandle, window: &Window, cx: &gpui::App) -> bool {
        h.contains_focused(window, cx)
    }

    fn area(
        &self,
        id: &'static str,
        s: &Entity<TextareaState>,
        f: Field,
        window: &Window,
        cx: &gpui::App,
    ) -> Div {
        let on = self.focused(s.read(cx).focus_handle(cx), window, cx);
        plate(
            id,
            on,
            !self.errors_of(f).is_empty(),
            Textarea::new(s).appearance(false).bordered(false),
        )
    }

    fn line(
        &self,
        id: &'static str,
        s: &Entity<InputState>,
        f: Field,
        window: &Window,
        cx: &gpui::App,
    ) -> Div {
        let on = self.focused(s.read(cx).focus_handle(cx), window, cx);
        plate(
            id,
            on,
            !self.errors_of(f).is_empty(),
            Input::new(s).appearance(false).bordered(false),
        )
    }

    /// Label, an optional hint, the plate, and the field's own errors under it.
    fn labelled(&self, id: &'static str, label: &str, hint: &str, f: Field, plate: Div) -> Div {
        div()
            .debug_selector(move || format!("{id}-field"))
            .flex()
            .flex_col()
            .gap(px(t::S2))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(t::S2))
                    .child(text(t::T2, t::W_MEDIUM, t::TEXT_STRONG).child(label.to_string()))
                    .child(text(t::T2, t::W_REGULAR, t::TEXT).child(hint.to_string())),
            )
            .child(plate)
            .children(self.errors_of(f).into_iter().map(|e| {
                text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                    .debug_selector(move || format!("{id}-error"))
                    .whitespace_normal()
                    .child(e.to_string())
            }))
    }

    fn folder(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let writes = self.kind == GoalKind::Writes;
        let plate = self.line("sa-goal-folder", &self.ui.folder, Field::Folder, window, cx);
        let row = div()
            .flex()
            .items_center()
            .gap(px(t::S2))
            .child(div().flex_1().min_w(px(0.)).child(plate))
            .children((!self.remote).then(|| {
                kit::button(
                    "sa-goal-choose",
                    "Choose folder...",
                    ButtonKind::Secondary,
                    self.stops.choose.is_focused(window),
                    Some(Box::new(
                        cx.listener(|v, _, window, cx| v.choose_folder(window, cx)),
                    )),
                )
                .track_focus(&self.stops.choose)
            }));
        let mut d = self.labelled(
            "sa-goal-folder",
            "Folder",
            if writes {
                "a git repository (absolute path)"
            } else {
                "any directory (absolute path)"
            },
            Field::Folder,
            div().child(row),
        );
        if writes {
            let (base, warn) = self.head_text();
            d = d.child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(t::S2))
                    .child(text(t::T2, t::W_MEDIUM, t::TEXT).child("Base commit"))
                    .child(
                        text(
                            t::T2,
                            t::W_REGULAR,
                            if warn { t::SOFT_BEIGE } else { t::TEXT_STRONG },
                        )
                        .debug_selector(|| "sa-goal-base".to_string())
                        .font_family(crate::brand::MONO_FONT)
                        .whitespace_normal()
                        .child(base),
                    ),
            );
        } else {
            d = d.child(
                text(t::T2, t::W_REGULAR, t::TEXT)
                    .debug_selector(|| "sa-goal-folder-note".to_string())
                    .whitespace_normal()
                    .child(READ_ONLY_FOLDER_NOTE),
            );
        }
        d
    }

    fn fields(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let writes = self.kind == GoalKind::Writes;
        let u = &self.ui;
        let mut d = div()
            .flex()
            .flex_col()
            .gap(px(t::S4))
            .child(self.labelled(
                "sa-goal-goal",
                "Goal",
                if writes {
                    "what must be true when it is done"
                } else {
                    "what the report must find out"
                },
                Field::Goal,
                self.area("sa-goal-goal", &u.goal, Field::Goal, window, cx),
            ))
            .child(self.labelled(
                "sa-goal-criteria",
                if writes {
                    "Acceptance criteria"
                } else {
                    "Rubric"
                },
                if writes {
                    "one per line; each must be met"
                } else {
                    "optional, one per line; you check the report against them"
                },
                Field::Criteria,
                self.area("sa-goal-criteria", &u.criteria, Field::Criteria, window, cx),
            ));
        if writes {
            d = d
                .child(self.labelled(
                    "sa-goal-checks",
                    "Checks",
                    "verification commands, one per line; they are run independently",
                    Field::Checks,
                    self.area("sa-goal-checks", &u.checks, Field::Checks, window, cx),
                ))
                .child(self.labelled(
                    "sa-goal-scope",
                    "Write scope",
                    "path patterns attempts may change, one per line",
                    Field::Scope,
                    self.area("sa-goal-scope", &u.scope, Field::Scope, window, cx),
                ));
        }
        d = d.child(self.labelled(
            "sa-goal-budget",
            "Budget (USD)",
            "the most you accept to spend; no default",
            Field::Budget,
            self.line("sa-goal-budget", &u.budget, Field::Budget, window, cx),
        ));
        d = d.child(self.labelled(
            "sa-goal-turns",
            "Turns per attempt",
            &match self.turn_setting {
                Some(n) => format!("model calls, 1 to 200; empty means {n} (Settings)"),
                None => "model calls, 1 to 200; empty means the setting".to_string(),
            },
            Field::Turns,
            self.line("sa-goal-turns", &u.turns, Field::Turns, window, cx),
        ));
        if !writes {
            d = d.child(self.labelled(
                "sa-goal-attempts",
                "Attempts",
                "optional, 1 to 10; empty means 3",
                Field::Attempts,
                self.line("sa-goal-attempts", &u.attempts, Field::Attempts, window, cx),
            ));
        }
        d.child(self.folder(window, cx))
    }

    fn actions(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let why = self.why_not_send(cx);
        let sending = self.form.stage == Stage::Sending;
        let retry = matches!(self.screen, Screen::Unreachable(_) | Screen::NotReady(_));
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(t::S2))
            .mt(px(t::S4))
            .child(
                kit::button(
                    "sa-goal-send",
                    if sending {
                        "Sending..."
                    } else {
                        "Create draft"
                    },
                    if why.is_some() {
                        ButtonKind::Disabled
                    } else {
                        ButtonKind::Primary
                    },
                    self.stops.send.is_focused(window),
                    Some(Box::new(cx.listener(|v, e: &gpui::ClickEvent, _, cx| {
                        if !e.is_keyboard() {
                            v.send(cx)
                        }
                    }))),
                )
                .track_focus(&self.stops.send),
            )
            .child(
                kit::button(
                    "sa-goal-cancel",
                    "Cancel",
                    ButtonKind::Ghost,
                    self.stops.cancel.is_focused(window),
                    Some(Box::new(cx.listener(|v, _, _, cx| v.close(cx)))),
                )
                .track_focus(&self.stops.cancel),
            )
            .children(retry.then(|| {
                kit::button(
                    "sa-goal-retry",
                    "Try again",
                    ButtonKind::Secondary,
                    false,
                    Some(Box::new(cx.listener(|v, _, _, cx| v.retry_link(cx)))),
                )
            }))
            .child(
                text(t::T2, t::W_REGULAR, t::TEXT)
                    .debug_selector(|| "sa-goal-why".to_string())
                    .child(match why {
                        Some(w) => w.to_string(),
                        None => format!(
                            "{}+Enter creates the draft. Nothing runs until you approve it.",
                            mod_name()
                        ),
                    }),
            )
    }

    fn page(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let writes = self.kind == GoalKind::Writes;
        let head = div()
            .flex()
            .flex_col()
            .gap(px(t::S2))
            .child(text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG).child(if writes {
                "New goal (writes)"
            } else {
                "New read-only goal"
            }))
            .child(text(t::T2, t::W_REGULAR, t::TEXT).child(if writes {
                "Works in a git worktree. You approve a contract before anything runs."
            } else {
                "Reads only. Returns a report you accept."
            }));
        if let Some(why) = self.blocked() {
            return div()
                .flex()
                .flex_col()
                .gap(px(t::S5))
                .child(head)
                .child(
                    kit::card("sa-goal-blocked", true).child(
                        text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                            .whitespace_normal()
                            .child(why),
                    ),
                )
                .child(div().child(kit::button(
                    "sa-goal-cancel",
                    "Close",
                    ButtonKind::Secondary,
                    false,
                    Some(Box::new(cx.listener(|v, _, _, cx| v.close(cx)))),
                )))
                .into_any_element();
        }
        let notice = match &self.screen {
            Screen::Unreachable(e) | Screen::NotReady(e) => Some(e.clone()),
            _ => None,
        };
        let loose: Vec<String> = self.loose_errors().into_iter().map(String::from).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(t::S5))
            .child(head)
            .children(notice.map(|n| {
                text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                    .debug_selector(|| "sa-goal-notice".to_string())
                    .whitespace_normal()
                    .child(n)
            }))
            .children((!loose.is_empty()).then(|| {
                div()
                    .debug_selector(|| "sa-goal-errors".to_string())
                    .flex()
                    .flex_col()
                    .gap(px(t::S1))
                    .children(loose.into_iter().map(|e| {
                        text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                            .whitespace_normal()
                            .child(e)
                    }))
            }))
            .child(self.fields(window, cx))
            .child(self.actions(window, cx))
            .into_any_element()
    }

    /// The first field takes the keys when the form shows; a review starts with nothing focused, so the Enter that
    /// sent the form can never land on a button of the review.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reviewing = matches!(self.form.stage, Stage::Review(_) | Stage::Approving(_));
        if std::mem::take(&mut self.want_focus) && !reviewing {
            let first = self.ui.goal.read(cx).focus_handle(cx);
            window.defer(cx, move |window, cx| window.focus(&first, cx));
        }
        if reviewing && !self.was_reviewing {
            let root = self.focus.clone();
            window.defer(cx, move |window, cx| window.focus(&root, cx));
        }
        self.was_reviewing = reviewing;
    }
}

impl Render for GoalForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_focus(window, cx);
        let body = match &self.form.stage {
            Stage::Review(_) | Stage::Approving(_) => review::page(self, window, cx),
            _ => self.page(window, cx),
        };
        div()
            .id("sa-goal")
            .debug_selector(|| "sa-goal".to_string())
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev, window, cx| {
                if v.on_key(ev, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .id("sa-goal-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(728.))
                            .mx_auto()
                            .px(px(t::S5))
                            .py(px(t::S6))
                            .child(body),
                    ),
            )
            .children(review::dialog(self, window, cx))
    }
}
