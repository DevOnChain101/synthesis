//! Needs you: one row per decision and per waiting contract in the server's order (highest tier first), and one per
//! session that asks for a tool. With an older orchestrator, one row per outcome, oldest first. The row: status glyph,
//! title, detail, age, expiry, what it blocks, the risk chip and Review (Open for a session). Review opens the
//! decision card in place of the row. Below: "Lapsed" and "Answered, recorded", collapsed.

use gpui::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, px};

use super::data::{Model, NeedRow};
use super::{HomeView, RowSpec, row, section};
use crate::approvals::Kind;
use crate::superagent::kit::{self, ButtonKind, Status};
use crate::superagent::tokens as t;
use synthesis_view::outcomes::Tier;

impl HomeView {
    pub(super) fn needs(&self, m: &Model, cx: &mut Context<Self>) -> AnyElement {
        let open = self.open.as_ref();
        let limit = if self.show_all || open.is_some() {
            m.needs.len()
        } else {
            super::FIRST
        };
        // the one row the open card belongs to
        let opened = open.and_then(|o| {
            m.needs.iter().position(|r| {
                r.key == o.outcome
                    && o.decision
                        .as_ref()
                        .is_none_or(|d| r.decision.as_ref() == Some(d))
            })
        });
        // a row that must be confirmed is never folded away
        let mut rows: Vec<AnyElement> = m
            .needs
            .iter()
            .enumerate()
            .filter(|(i, r)| *i < limit || r.tier == Some(Tier::T3))
            .map(|(i, r)| match open.filter(|_| opened == Some(i)) {
                Some(o) if o.popup.is_some() => self.card(o, r, cx),
                o => self.need_row(i, r, o.is_some(), cx),
            })
            .collect();
        let hidden = m.needs.len() - rows.len();
        if m.needs.is_empty() {
            rows.push(
                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                    .px(px(t::S4))
                    .py(px(t::S2))
                    .child("Nothing waits for you. Super-Agent keeps working.")
                    .into_any_element(),
            );
        }
        if hidden > 0 {
            rows.push(
                kit::button(
                    "home-more",
                    format!("Show {hidden} more"),
                    ButtonKind::Ghost,
                    false,
                    Some(Box::new(cx.listener(|v, _, _, cx| {
                        v.show_all = true;
                        cx.notify();
                    }))),
                )
                .self_start()
                .into_any_element(),
            );
        }
        section("home-needs", "Needs you", m.needs.len(), rows).into_any_element()
    }

    fn need_row(&self, i: usize, r: &NeedRow, reading: bool, cx: &mut Context<Self>) -> AnyElement {
        let tool = r.kind == Kind::Tool;
        let mut meta = vec![r.detail.clone(), r.age.clone()];
        meta.extend(r.expires.clone());
        meta.retain(|s| !s.is_empty());
        let mut trailing = vec![];
        // the one-tap slot: only ever drawn when the gate is open, the answer is recommended and the risk is low
        if r.one_tap(self.gates) {
            // approvals-concept A1/A4 (ship later): the recommended answer's button goes here
        }
        if let Some(t) = r.tier {
            trailing.push(kit::composer_chip("home-risk", t.chip()).into_any_element());
        }
        trailing.push(if reading {
            kit::text(t::T2, t::W_REGULAR, t::TEXT)
                .child("Reading the brief...")
                .into_any_element()
        } else {
            kit::button(
                "home-review",
                if tool { "Open" } else { "Review" },
                ButtonKind::Ghost,
                false,
                Some(Box::new(cx.listener(move |v, _, _, cx| {
                    v.activate(super::Target::Need(i), cx)
                }))),
            )
            .into_any_element()
        });
        row(
            ("home-need", i),
            RowSpec {
                status: Status::Needs,
                title: r.title.clone(),
                meta,
                notes: r.blocks.iter().cloned().collect(),
                trailing,
                current: self.cursor == Some(i),
            },
            Some(Box::new(cx.listener(move |v, _, _, cx| {
                v.activate(super::Target::Need(i), cx)
            }))),
        )
        .into_any_element()
    }

    /// "Lapsed" and "Answered, recorded": nothing to answer there, so each is a count until it is opened.
    pub(super) fn logs(&self, m: &Model, cx: &mut Context<Self>) -> Vec<AnyElement> {
        [
            (
                "home-lapsed",
                "Lapsed",
                Status::Draft,
                &m.lapsed,
                self.show_lapsed,
            ),
            (
                "home-answered",
                "Answered, recorded",
                Status::Verified,
                &m.answered,
                self.show_answered,
            ),
        ]
        .into_iter()
        .filter(|(_, _, _, log, _)| !log.is_empty())
        .map(|(id, title, status, log, shown)| {
            let toggle = kit::button(
                id,
                if shown { "Hide" } else { "Show" },
                ButtonKind::Ghost,
                false,
                Some(Box::new(cx.listener(move |v, _, _, cx| {
                    let on = if id == "home-lapsed" {
                        &mut v.show_lapsed
                    } else {
                        &mut v.show_answered
                    };
                    *on = !*on;
                    cx.notify();
                }))),
            )
            .self_start()
            .into_any_element();
            let mut rows = vec![toggle];
            if shown {
                rows.extend(log.iter().enumerate().map(|(i, r)| {
                    row(
                        (id, i),
                        RowSpec {
                            status,
                            title: r.title.clone(),
                            meta: r.meta.clone(),
                            notes: vec![],
                            trailing: vec![],
                            current: false,
                        },
                        None,
                    )
                    .into_any_element()
                }));
            }
            section(id, title, log.len(), rows).into_any_element()
        })
        .collect()
    }
}
