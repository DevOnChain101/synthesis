//! `/rewind` in the window: the three steps (list, dry run, apply) run off the UI thread; their
//! results go back to `Live` (`crate::rewind`). After an apply the Files tab is read again.

use gpui::{AppContext as _, Context};

use super::TuiView;
use crate::live::Action;
use crate::rewind;

impl TuiView {
    pub(super) fn run_rewind(&mut self, a: Action, cx: &mut Context<Self>) {
        let Some(lu) = &mut self.live else { return };
        let Some(e) = lu.live.engine.clone() else {
            return;
        };
        let home = rewind::Homes {
            synthesis: e.synthesis_home.clone(),
            user: synthesis_app::homes(&lu.cfg.env).ok().map(|h| h.user),
        };
        let sid = e.sid.clone();
        match a {
            Action::Rewind => {
                let job = cx.background_spawn(async move { rewind::list(&home, &sid) });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| t.live_do(cx, |l| l.rewind_listed(r)))
                        .ok();
                })
                .detach();
            }
            Action::RewindPreview(cp) => {
                let job = cx.background_spawn(async move { rewind::preview(&home, &sid, cp) });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| t.live_do(cx, |l| l.rewind_previewed(r)))
                        .ok();
                })
                .detach();
            }
            Action::RewindApply(p) => {
                lu.live.rewinding = true;
                let job = cx.background_spawn(async move { rewind::apply(&home, &sid, &p) });
                cx.spawn(async move |this, cx| {
                    let r = job.await;
                    this.update(cx, |t, cx| {
                        t.live_do(cx, |l| l.rewind_done(r));
                        // the Files tab reads the folder again; a preview of a file that is back is stale
                        t.ts.preview = None;
                        t.request_scan(cx);
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

    /// A click on "Show more" / "Show less" of group `g` of the rewind confirmation.
    pub(super) fn rewind_toggle(&mut self, g: usize, cx: &mut Context<Self>) {
        self.live_do(cx, |l| l.rewind_toggle(g));
    }
}
