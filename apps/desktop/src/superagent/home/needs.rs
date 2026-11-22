//! Needs you: one row per outcome with decisions pending (and per session that asks for a tool), oldest first.
//! The row: status glyph, title, outcome detail, age, "blocks wN" when the model has it, and Review (Open for a
//! session). Review opens the decision card in place of the row.

use gpui::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, px};

use super::data::{Model, NeedRow};
use super::{HomeView, RowSpec, row, section};
use crate::approvals::Kind;
use crate::superagent::kit::{self, ButtonKind, Status};
use crate::superagent::tokens as t;

impl HomeView {
    pub(super) fn needs(&self, m: &Model, cx: &mut Context<Self>) -> AnyElement {
        let open = self.open.as_ref();
        let limit = if self.show_all || open.is_some() {
            m.needs.len()
        } else {
            super::FIRST
        };
        let mut rows: Vec<AnyElement> = m
            .needs
            .iter()
            .enumerate()
            .take(limit)
            .map(|(i, r)| match open.filter(|o| o.outcome == r.key) {
                Some(o) if o.popup.is_some() => self.card(o, r, cx),
                o => self.need_row(i, r, o.is_some(), cx),
            })
            .collect();
        if m.needs.is_empty() {
            rows.push(
                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                    .px(px(t::S4))
                    .py(px(t::S2))
                    .child("Nothing waits for you. Super-Agent keeps working.")
                    .into_any_element(),
            );
        }
        if m.needs.len() > limit {
            rows.push(
                kit::button(
                    "home-more",
                    format!("Show {} more", m.needs.len() - limit),
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
        meta.retain(|s| !s.is_empty());
        let mut trailing = vec![];
        // the one-tap slot: only ever drawn when the gate is open, the answer is recommended and the risk is low
        if r.one_tap(self.gates) {
            // approvals-concept A1/A4 (ship later): the recommended answer's button goes here
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
                notes: r.blocks.iter().map(|b| format!("blocks {b}")).collect(),
                trailing,
                current: self.cursor == Some(i),
            },
            Some(Box::new(cx.listener(move |v, _, _, cx| {
                v.activate(super::Target::Need(i), cx)
            }))),
        )
        .into_any_element()
    }
}
