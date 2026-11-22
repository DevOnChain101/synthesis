//! Finished: outcomes that ended (verified or failed), newest as the server lists them.

use gpui::{AnyElement, Context, IntoElement as _};

use super::data::Model;
use super::{HomeView, RowSpec, Target, row, section};

impl HomeView {
    pub(super) fn finished(&self, m: &Model, cx: &mut Context<Self>) -> AnyElement {
        let base = m.needs.len() + m.running.len();
        let rows = m
            .finished
            .iter()
            .enumerate()
            .map(|(i, r)| {
                row(
                    ("home-fin", i),
                    RowSpec {
                        status: r.status,
                        title: r.title.clone(),
                        meta: r.meta.clone(),
                        notes: r.notes.clone(),
                        trailing: vec![],
                        current: self.cursor == Some(base + i),
                    },
                    Some(Box::new(
                        cx.listener(move |v, _, _, cx| v.activate(Target::Fin(i), cx)),
                    )),
                )
                .into_any_element()
            })
            .collect();
        section("home-finished", "Finished", m.finished.len(), rows).into_any_element()
    }
}
