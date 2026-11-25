//! The decision card, in place of the row on Review (port of `superagent::outcomes::render::card` and `card_layout`, with the
//! rules of #167 as moved by S0): nothing preselected, every field of the brief shown in the server's order, choose
//! and send are two acts, the whole brief must be opened before Send when text is hidden, and the keys act 400 ms
//! after the card got them. The state is `approvals::Popup`; this file only draws it.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use synthesis_view::outcomes::BriefRow;

use super::data::NeedRow;
use super::{HomeView, Open};
use crate::approvals::Popup;
use crate::superagent::outcomes::render::card_layout;
use crate::superagent::kit::{self, ButtonKind};
use crate::superagent::tokens::{self as t, hsla};

/// The collapsed brief keeps the first field to this much (the same cut as `Popup::first_cut`).
const CUT_LINES: usize = 2;
const CUT_CHARS: usize = 150;

fn cut(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut s = lines[..lines.len().min(CUT_LINES)].join("\n");
    if s.chars().count() > CUT_CHARS {
        s = s.chars().take(CUT_CHARS).collect();
    }
    if s.len() < text.len() {
        s.push_str(" ...");
    }
    s
}

/// The brief rows the card draws now: all of them when opened, else the first one, cut.
pub fn shown_rows(p: &Popup) -> Vec<BriefRow> {
    let rows = card_layout(&p.card).rows;
    if p.full {
        return rows;
    }
    rows.into_iter()
        .take(1)
        .map(|r| BriefRow {
            text: cut(&r.text),
            ..r
        })
        .collect()
}

/// The link that opens the rest, or `None` when nothing is hidden.
pub fn more_label(p: &Popup) -> Option<String> {
    let n = p.hidden_sections();
    if p.full {
        Some("Show less".into())
    } else if n > 0 {
        Some(format!(
            "Show {n} more section{}",
            if n == 1 { "" } else { "s" }
        ))
    } else {
        p.first_cut().then(|| "Show the full text".into())
    }
}

/// Why Send is off (shown next to it), or `None` when it would send.
pub fn why_not(p: &Popup, now: i64) -> Option<String> {
    let chosen = p.ask.picker.as_ref().and_then(|x| x.choice()).is_some();
    Some(
        if p.sent_at.is_some() || p.ask.sending {
            "Sending the answer..."
        } else if !chosen {
            "Choose an answer"
        } else if p.send_blocked() {
            "Open the whole brief first"
        } else if now < p.ask.armed_until {
            "Look at the decision first"
        } else {
            return None;
        }
        .into(),
    )
}

impl HomeView {
    pub(super) fn card(&self, o: &Open, r: &NeedRow, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = o.popup.as_ref() else {
            return div().into_any_element();
        };
        let now = self.now(cx);
        let lay = card_layout(&p.card);
        let off = why_not(p, now);
        let note = p.ask.notice.clone().filter(|n| !n.is_empty());
        let mut c = kit::card("home-card", true)
            .child(
                kit::text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                    .child(format!("Needs you, waiting {}", r.age)),
            )
            .child(kit::text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG).child(lay.title.clone()))
            .child(
                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                    .whitespace_normal()
                    .child(format!("{}. {} \u{b7} {}", r.title, lay.state, lay.meta)),
            );
        c = c.child(
            div()
                .debug_selector(|| "home-brief".to_string())
                .flex()
                .flex_col()
                .gap(px(t::S2))
                .p(px(t::S4))
                .rounded(px(t::R_CONTROL))
                .bg(hsla(t::BG))
                .children(shown_rows(p).into_iter().map(|b| {
                    div()
                        .flex()
                        .flex_col()
                        .child(kit::text(t::T2, t::W_MEDIUM, t::TEXT).child(b.label))
                        .child(
                            kit::text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                                .font_family(crate::brand::MONO_FONT)
                                .whitespace_normal()
                                .line_height(px(20.))
                                .child(b.text),
                        )
                })),
        );
        if let Some(l) = more_label(p) {
            c = c.child(
                kit::button(
                    "home-brief-toggle",
                    l,
                    ButtonKind::Ghost,
                    false,
                    Some(Box::new(cx.listener(|v, _, _, cx| {
                        if let Some(p) = v.open.as_mut().and_then(|o| o.popup.as_mut()) {
                            p.toggle_full();
                        }
                        cx.notify();
                    }))),
                )
                .self_start(),
            );
        }
        if let Some(pk) = &p.ask.picker {
            c = c.child(div().flex().flex_col().gap(px(t::S2)).children(
                pk.options.iter().enumerate().map(|(i, opt)| {
                    let on = pk.cursor() == Some(i);
                    div()
                        .id(("home-opt", i))
                        .debug_selector(move || format!("home-opt-{i}"))
                        .flex()
                        .items_center()
                        .gap(px(t::S4))
                        .min_h(px(40.))
                        .px(px(t::S4))
                        .py(px(t::S2))
                        .rounded(px(t::R_CONTROL))
                        .border_1()
                        .border_color(hsla(if on { t::ACCENT } else { t::BORDER }))
                        .when(on, |d| d.bg(hsla(t::ELEVATED)))
                        .cursor_pointer()
                        .hover(|d| d.bg(t::hover()))
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if let Some(p) = v.open.as_mut().and_then(|o| o.popup.as_mut()) {
                                p.ask.choose(i);
                            }
                            cx.notify();
                        }))
                        .child(kit::keycap("home-opt-key", (i + 1).to_string()))
                        .child(
                            kit::text(t::T3, t::W_REGULAR, t::TEXT_STRONG)
                                .whitespace_normal()
                                .child(if opt.label == opt.id {
                                    opt.id.clone()
                                } else {
                                    format!("{}  {}", opt.id, opt.label)
                                }),
                        )
                }),
            ));
        }
        c.child(
            kit::text(t::T1, t::W_REGULAR, t::TEXT)
                .font_family(crate::brand::MONO_FONT)
                .whitespace_normal()
                .child(lay.proof),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(t::S2))
                .child(kit::button(
                    "home-send",
                    "Send answer",
                    if off.is_some() {
                        ButtonKind::Disabled
                    } else {
                        ButtonKind::Primary
                    },
                    false,
                    Some(Box::new(cx.listener(|v, _, _, cx| v.send(cx)))),
                ))
                .child(
                    kit::text(
                        t::T2,
                        t::W_REGULAR,
                        if note.is_some() {
                            t::SOFT_BEIGE
                        } else {
                            t::TEXT
                        },
                    )
                    .debug_selector(|| "home-why".to_string())
                    .whitespace_normal()
                    .child(
                        note.or(off)
                            .unwrap_or_else(|| "Enter sends. Esc closes.".into()),
                    ),
                ),
        )
        .into_any_element()
    }
}
