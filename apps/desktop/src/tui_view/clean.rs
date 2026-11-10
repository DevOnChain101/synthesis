//! `/clean` in the window: the counts and the deletion run off the UI thread; their results go back to
//! `Live` (`crate::clean`). After a deletion the Workspace / Running / Recent lists are read again.

use gpui::{AppContext as _, Context};

use super::TuiView;
use crate::clean;
use crate::live::Action;

impl TuiView {
    pub(super) fn run_clean(&mut self, a: Action, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        let home = e.synthesis_home.clone();
        // a counting step that never answers frees the prompt; a deletion keeps it blocked
        if matches!(a, Action::CleanApply { .. }) {
            lu.live.clean_deleting = true;
        }
        let token = lu.live.clean_job_start();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(clean::JOB_TIMEOUT_MS))
                .await;
            this.update(cx, |t, cx| t.live_do(cx, |l| l.clean_timeout(token)))
                .ok();
        })
        .detach();
        match a {
            Action::Clean => {
                let keep = vec![e.sid.clone()];
                let job = cx.background_spawn(async move { clean::plans(&home, &keep) });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| t.live_do(cx, |l| l.clean_listed(r)))
                        .ok();
                })
                .detach();
            }
            Action::CleanApply {
                scope,
                ids,
                backup,
                keep,
            } => {
                let job =
                    cx.background_spawn(
                        async move { clean::apply(&home, scope, &keep, &ids, backup) },
                    );
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| {
                        t.live_do(cx, |l| l.clean_done(r));
                        t.refresh_store(cx);
                    })
                    .ok();
                })
                .detach();
            }
            _ => {}
        }
        lu.dirty = true;
        cx.notify();
    }
}
