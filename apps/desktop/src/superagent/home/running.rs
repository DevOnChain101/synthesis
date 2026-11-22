//! Running: outcomes that are not finished, with the state word, the budget and a bar of the budget spent when the
//! server gave both numbers (the list carries no work-item counts), plus a beige note for what waits for the person.

use gpui::{AnyElement, Context, IntoElement as _, ParentElement as _, Styled as _, div, px};

use super::data::Model;
use super::{HomeView, RowSpec, Target, row, section};
use crate::superagent::kit;

impl HomeView {
    pub(super) fn running(&self, m: &Model, cx: &mut Context<Self>) -> AnyElement {
        let base = m.needs.len();
        let rows = m
            .running
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let bar = r.frac.map(|f| {
                    div()
                        .w(px(64.))
                        .child(kit::progress_bar("home-run-bar", f))
                        .into_any_element()
                });
                row(
                    ("home-run", i),
                    RowSpec {
                        status: r.status,
                        title: r.title.clone(),
                        meta: r.meta.clone(),
                        notes: r.notes.clone(),
                        trailing: bar.into_iter().collect(),
                        current: self.cursor == Some(base + i),
                    },
                    Some(Box::new(
                        cx.listener(move |v, _, _, cx| v.activate(Target::Run(i), cx)),
                    )),
                )
                .into_any_element()
            })
            .collect();
        section("home-running", "Running", m.running.len(), rows).into_any_element()
    }
}
