//! The review of what the server stored (version, hash, its diff, the contract) and the approve dialog of the mock.
//! Approving is two separate acts here: "Approve contract..." opens the dialog, and its own button approves exactly
//! the version and hash the server reported (`Form::approve`).

use std::time::Instant;

use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    Styled as _, Window, div, px,
};
use synthesis_harness::sanitize_for_display;
use synthesis_view::outcomes::CriteriaView;

use super::model::{approve_ok, turns_line};
use super::view::mod_name;
use super::{Dialog, GoalForm};
use crate::superagent::frames::GoalKind;
use crate::superagent::kit::{self, ButtonKind, text};
use crate::superagent::outcomes::form::{DiffView, Review, Stage};
use crate::superagent::tokens::{self as t, hsla};

fn kind_line(kind: GoalKind) -> &'static str {
    match kind {
        GoalKind::Writes => "Writes in a git worktree",
        GoalKind::ReadOnly => "Read-only. Never writes.",
    }
}

fn mono(s: impl Into<gpui::SharedString>, c: u32) -> Div {
    text(t::T2, t::W_REGULAR, c)
        .font_family(crate::brand::MONO_FONT)
        .whitespace_normal()
        .child(s.into())
}

/// The mock's `kv`: a label column and a value column.
fn kv(rows: Vec<(&'static str, Div)>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(t::S2))
        .children(rows.into_iter().map(|(k, v)| {
            div()
                .flex()
                .gap(px(t::S5))
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .flex_none()
                        .w(px(96.))
                        .child(k),
                )
                .child(v.flex_1().min_w(px(0.)))
        }))
}

fn plain(s: impl Into<gpui::SharedString>) -> Div {
    text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
        .whitespace_normal()
        .child(s.into())
}

fn warn(id: &'static str, head: &str, items: &[String]) -> Option<Div> {
    (!items.is_empty()).then(|| {
        kit::card(id, true)
            .child(
                text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                    .whitespace_normal()
                    .child(head.to_string()),
            )
            .children(items.iter().map(|i| plain(format!("- {i}"))))
    })
}

fn diff(d: &DiffView) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(t::S2))
        .child(text(t::T3, t::W_MEDIUM, t::TEXT_STRONG).child(format!(
            "Changes against the approved contract (version {})",
            d.base_version
        )))
        .children((d.changes.is_empty()).then(|| plain("No field differs.")))
        .children(d.changes.iter().map(|c| {
            div()
                .flex()
                .flex_col()
                .child(text(t::T2, t::W_REGULAR, t::TEXT).child(format!("{} {}", c.area, c.key)))
                .child(kv(vec![
                    ("was", plain(c.old.clone())),
                    ("now", plain(c.new.clone())),
                ]))
        }))
}

fn contract(c: &CriteriaView) -> Div {
    let mut d = div()
        .flex()
        .flex_col()
        .gap(px(t::S2))
        .child(kv(vec![("Goal", plain(c.goal.clone()))]))
        .child(text(t::T3, t::W_MEDIUM, t::TEXT_STRONG).child("Success criteria"));
    for k in &c.criteria {
        d = d.child(
            div()
                .flex()
                .gap(px(t::S5))
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .flex_none()
                        .w(px(96.))
                        .child(k.id.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(plain(k.text.clone()))
                        .child(
                            text(t::T2, t::W_REGULAR, t::TEXT)
                                .whitespace_normal()
                                .child(format!(
                                    "{} \u{b7} trust {} \u{b7} checks {}",
                                    if k.manual == Some(true) {
                                        "you review it"
                                    } else {
                                        "verified by checks"
                                    },
                                    k.required_trust,
                                    if k.check_ids.is_empty() {
                                        "none".to_string()
                                    } else {
                                        k.check_ids.join(", ")
                                    }
                                )),
                        ),
                ),
        );
    }
    if !c.checks.is_empty() {
        d = d
            .child(text(t::T3, t::W_MEDIUM, t::TEXT_STRONG).child("Independent checks"))
            .children(c.checks.iter().map(|k| mono(k.clone(), t::TEXT_STRONG)));
    }
    d
}

fn review_body(f: &GoalForm, r: &Review) -> Div {
    let mut d =
        div()
            .flex()
            .flex_col()
            .gap(px(t::S4))
            .child(kit::card("sa-goal-stored", false).child(kv(vec![
                ("Kind", plain(kind_line(f.kind))),
                (
                    "Turns",
                    plain(turns_line(
                        r.contract.as_ref().ok().and_then(|c| c.max_turns),
                        f.turn_setting,
                    )),
                ),
                ("Version", plain(r.version.to_string())),
                ("Hash", mono(r.hash.clone(), t::TEXT_STRONG)),
            ])));
    match &r.diff {
        None => {}
        Some(Ok(x)) => {
            d = d
                .children(warn(
                    "sa-goal-widens",
                    "WARNING: this version gives attempts MORE room than the approved contract",
                    &x.widens,
                ))
                .children(warn(
                    "sa-goal-weakens",
                    "WARNING: this version lowers the bar for done",
                    &x.weakens,
                ))
                .child(kit::card("sa-goal-diff", false).child(diff(x)));
        }
        Some(Err(e)) => {
            d = d.child(
                text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                    .whitespace_normal()
                    .child(format!("The server's diff cannot be shown, so this contract cannot be approved here: {e}")),
            )
        }
    }
    match &r.contract {
        Ok(c) => d.child(kit::card("sa-goal-contract", false).child(contract(c))),
        Err(e) => d.child(
            text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                .whitespace_normal()
                .child(format!(
                    "The stored contract cannot be shown, so it cannot be approved here: {e}"
                )),
        ),
    }
}

pub(super) fn page(f: &GoalForm, window: &Window, cx: &mut Context<GoalForm>) -> AnyElement {
    let (Stage::Review(r) | Stage::Approving(r)) = &f.form.stage else {
        return div().into_any_element();
    };
    let approving = matches!(f.form.stage, Stage::Approving(_));
    let loose: Vec<String> = f.form.errors.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(t::S5))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(t::S2))
                .child(text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG).child("Review the draft"))
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT).whitespace_normal().child(
                        "The draft is stored. Its contract is NOT approved: nothing runs until you approve it.",
                    ),
                ),
        )
        .children((!loose.is_empty()).then(|| {
            div()
                .debug_selector(|| "sa-goal-errors".to_string())
                .flex()
                .flex_col()
                .gap(px(t::S1))
                .children(loose.into_iter().map(|e| {
                    text(t::T2, t::W_REGULAR, t::SOFT_BEIGE).whitespace_normal().child(e)
                }))
        }))
        .child(review_body(f, r))
        .child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(t::S2))
                .child(
                    kit::button(
                        "sa-goal-review-approve",
                        if approving { "Approving..." } else { "Approve contract..." },
                        if r.approvable() && !approving { ButtonKind::Primary } else { ButtonKind::Disabled },
                        f.stops.review_approve.is_focused(window),
                        Some(Box::new(cx.listener(|v, e: &gpui::ClickEvent, window, cx| {
                            if !e.is_keyboard() {
                                v.open_dialog(window, cx)
                            }
                        }))),
                    )
                    .track_focus(&f.stops.review_approve),
                )
                .child(
                    kit::button(
                        "sa-goal-review-close",
                        "Not now",
                        ButtonKind::Ghost,
                        f.stops.review_close.is_focused(window),
                        Some(Box::new(cx.listener(|v, _, _, cx| v.close(cx)))),
                    )
                    .track_focus(&f.stops.review_close),
                )
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .whitespace_normal()
                        .child(format!(
                            "Approving sends version {} with the hash above. Nothing is approved until you press Approve in the dialog.",
                            r.version
                        )),
                ),
        )
        .into_any_element()
}

/// The mock's approve dialog over the page: w 480, p 8, r 12, opaque.
pub(super) fn dialog(
    f: &GoalForm,
    window: &Window,
    cx: &mut Context<GoalForm>,
) -> Option<AnyElement> {
    let Dialog { since } = f.dialog.as_ref()?;
    let (Stage::Review(r) | Stage::Approving(r)) = &f.form.stage else {
        return None;
    };
    let approving = matches!(f.form.stage, Stage::Approving(_));
    let armed = approve_ok(*since, Instant::now(), false, false, false);
    let typed = f.text(cx);
    let mut rows = vec![
        (
            "Goal",
            plain(
                r.contract
                    .as_ref()
                    .map_or_else(|_| String::new(), |c| c.goal.clone()),
            ),
        ),
        ("Kind", plain(kind_line(f.kind))),
        (
            "Budget",
            plain(sanitize_for_display(&format!("${}", typed.budget.trim()))),
        ),
        (
            "Turns",
            plain(turns_line(
                r.contract.as_ref().ok().and_then(|c| c.max_turns),
                f.turn_setting,
            )),
        ),
        (
            "Folder",
            mono(sanitize_for_display(typed.workspace.trim()), t::TEXT_STRONG),
        ),
    ];
    if f.kind == GoalKind::Writes {
        rows.push((
            "Base commit",
            mono(sanitize_for_display(&f.head_text().0), t::TEXT_STRONG),
        ));
    }
    rows.push(("Version", plain(r.version.to_string())));
    rows.push((
        "Hash",
        mono(
            r.hash.chars().take(16).collect::<String>() + "\u{2026}",
            t::TEXT_STRONG,
        ),
    ));
    let panel = div()
        .debug_selector(|| "sa-goal-dialog".to_string())
        .w(px(480.))
        .max_w_full()
        .p(px(t::S2))
        .rounded(px(t::R_PANEL))
        .bg(hsla(t::ELEVATED))
        .border_1()
        .border_color(t::stroke())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(t::S4))
                .p(px(t::S4))
                .child(text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG).child(format!("Approve contract v{}", r.version)))
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT).whitespace_normal().child(match f.kind {
                        GoalKind::Writes => "Checks decide whether this goal is met. Approving starts planning; nothing runs before.",
                        GoalKind::ReadOnly => "You accept the report; nothing is written. Approving starts the work; nothing runs before.",
                    }),
                )
                .child(kv(rows))
                .children((f.kind == GoalKind::ReadOnly).then(|| {
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .debug_selector(|| "sa-goal-dialog-note".to_string())
                        .whitespace_normal()
                        .child(super::view::READ_ONLY_FOLDER_NOTE)
                }))
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .debug_selector(|| "sa-goal-dialog-why".to_string())
                        .whitespace_normal()
                        .child(if approving {
                            "Approving...".to_string()
                        } else if !armed {
                            "Read the details first.".to_string()
                        } else {
                            format!("Tab to Approve, then Enter. {}+Enter never approves.", mod_name())
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(t::S2))
                        .child(
                            kit::button(
                                "sa-goal-dialog-cancel",
                                "Cancel",
                                ButtonKind::Ghost,
                                f.stops.dlg_cancel.is_focused(window),
                                Some(Box::new(cx.listener(|v, _, window, cx| v.close_dialog(window, cx)))),
                            )
                            .track_focus(&f.stops.dlg_cancel),
                        )
                        .child(
                            kit::button(
                                "sa-goal-dialog-approve",
                                "Approve contract",
                                if armed && !approving { ButtonKind::Primary } else { ButtonKind::Disabled },
                                f.stops.dlg_approve.is_focused(window),
                                Some(Box::new(cx.listener(|v, e: &gpui::ClickEvent, window, cx| {
                                    // Space or Enter key-up on the focused button is a keyboard click: `on_key`
                                    // is the only keyboard path (it knows held keys and the Enter-only rule)
                                    if !e.is_keyboard() {
                                        v.approve(false, false, window, cx)
                                    }
                                }))),
                            )
                            .track_focus(&f.stops.dlg_approve),
                        ),
                ),
        );
    Some(
        div()
            .debug_selector(|| "sa-goal-scrim".to_string())
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(104.))
            .px(px(t::S4))
            .bg(t::scrim())
            .child(panel)
            .into_any_element(),
    )
}
