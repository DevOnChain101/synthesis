//! Live view (spec `docs/specs/terminal-browser.md` 2.3): Chrome's `Page.startScreencast` frames land in ONE
//! slot that keeps only the newest. They never enter the CDP event broadcast (so a frame flood cannot lag
//! the policy listener), the capture logs, the event stream or the store, and never reach the model.
//! `Page.screencastFrameAck` goes out only when the consumer takes a frame, so Chrome slows down to it.

use crate::cdp::{Cdp, CdpEvent};
use base64::Engine;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The consumer gets at most 30 frames per second, because acks are not sent faster.
const MIN_INTERVAL: Duration = Duration::from_millis(33);
/// Chrome keeps at most two frames in flight; this only bounds the bookkeeping if that ever changes.
const MAX_UNACKED: usize = 8;

/// Where the frame sits in the page, as Chrome reports it (CSS pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameMeta {
    pub device_width: f64,
    pub device_height: f64,
    pub page_scale_factor: f64,
    pub offset_top: f64,
    pub scroll_x: f64,
    pub scroll_y: f64,
    /// Seconds since the epoch, from Chrome (0 when Chrome sent none).
    pub timestamp: f64,
    /// Device pixel ratio of the emulated viewport (1 until the view follows the window, B3).
    pub device_pixel_ratio: f64,
}

#[derive(Clone, Debug)]
pub struct Frame {
    /// Counts frames received from Chrome since launch; gaps mean frames the consumer was too slow for.
    pub seq: u64,
    pub jpeg: Vec<u8>,
    pub meta: FrameMeta,
}

struct Raw {
    seq: u64,
    ack_id: i64,
    data: String,
    meta: FrameMeta,
}

#[derive(Default)]
struct State {
    /// Session of the tab being cast; frames of any other session are dropped.
    active: Option<String>,
    frame: Option<Raw>,
    /// Frames replaced before the consumer saw them: acked together with the next one taken.
    unacked: Vec<i64>,
    last_take: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Slot {
    state: Mutex<State>,
    notify: Notify,
    pub(crate) received: AtomicU64,
}

fn num(v: &serde_json::Value, k: &str) -> f64 {
    v[k].as_f64().unwrap_or(0.0)
}

impl Slot {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Called by the CDP dispatcher for every `Page.screencastFrame`; keeps only the newest.
    pub(crate) fn push(&self, e: &CdpEvent) {
        let mut st = self.state();
        if st.active.is_none() || st.active != e.session_id {
            return;
        }
        let (Some(ack_id), Some(data)) =
            (e.params["sessionId"].as_i64(), e.params["data"].as_str())
        else {
            return;
        };
        let m = &e.params["metadata"];
        let seq = self.received.fetch_add(1, Ordering::SeqCst) + 1;
        let new = Raw {
            seq,
            ack_id,
            data: data.to_owned(),
            meta: FrameMeta {
                device_width: num(m, "deviceWidth"),
                device_height: num(m, "deviceHeight"),
                page_scale_factor: num(m, "pageScaleFactor"),
                offset_top: num(m, "offsetTop"),
                scroll_x: num(m, "scrollOffsetX"),
                scroll_y: num(m, "scrollOffsetY"),
                timestamp: num(m, "timestamp"),
                device_pixel_ratio: 1.0,
            },
        };
        if let Some(old) = st.frame.replace(new)
            && st.unacked.len() < MAX_UNACKED
        {
            st.unacked.push(old.ack_id);
        }
        drop(st);
        self.notify.notify_one();
    }

    /// Starts accepting frames of `session`; returns the session that was cast before, if any.
    pub(crate) fn activate(&self, session: Option<String>) -> Option<String> {
        let mut st = self.state();
        st.frame = None;
        st.unacked.clear();
        std::mem::replace(&mut st.active, session)
    }
}

/// The consumer end of the slot (cheap to clone; all clones share the one slot).
#[derive(Clone)]
pub struct FrameStream {
    pub(crate) slot: Arc<Slot>,
    pub(crate) cdp: Weak<Cdp>,
}

impl FrameStream {
    /// Waits for the newest frame and takes it. This is the ack: Chrome sends the next frame only now.
    pub async fn next(&self) -> Frame {
        loop {
            let wait = self
                .slot
                .state()
                .last_take
                .map(|t| (t + MIN_INTERVAL).saturating_duration_since(Instant::now()));
            if let Some(w) = wait.filter(|w| !w.is_zero()) {
                tokio::time::sleep(w).await;
            }
            let taken = {
                let mut st = self.slot.state();
                let raw = st.frame.take();
                raw.map(|r| {
                    st.last_take = Some(Instant::now());
                    let mut ids = std::mem::take(&mut st.unacked);
                    ids.push(r.ack_id);
                    (r, ids, st.active.clone())
                })
            };
            let Some((raw, ids, session)) = taken else {
                self.slot.notify.notified().await;
                continue;
            };
            if let (Some(cdp), Some(session)) = (self.cdp.upgrade(), session) {
                tokio::spawn(async move {
                    for id in ids {
                        let _ = cdp
                            .send(
                                Some(&session),
                                "Page.screencastFrameAck",
                                serde_json::json!({"sessionId": id}),
                            )
                            .await;
                    }
                });
            }
            // Bad base64 from the browser: skip the frame (it was acked) and wait for the next.
            if let Ok(jpeg) = base64::engine::general_purpose::STANDARD.decode(&raw.data) {
                return Frame {
                    seq: raw.seq,
                    jpeg,
                    meta: raw.meta,
                };
            }
        }
    }

    /// Frames Chrome has sent since launch (to observe that it stops when nobody consumes).
    pub fn received(&self) -> u64 {
        self.slot.received.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(session: &str, ack: i64, data: &str) -> CdpEvent {
        CdpEvent {
            method: "Page.screencastFrame".into(),
            params: json!({"data": data, "sessionId": ack, "metadata": {"deviceWidth": 800, "offsetTop": 3}}),
            session_id: Some(session.into()),
        }
    }

    #[test]
    fn keeps_only_the_newest_and_ignores_other_sessions_and_inactive() {
        let s = Slot::default();
        s.push(&ev("A", 1, "aGk=")); // not active yet
        assert!(s.state().frame.is_none());
        s.activate(Some("A".into()));
        s.push(&ev("B", 9, "aGk="));
        s.push(&ev("A", 1, "aGk="));
        s.push(&ev("A", 2, "aGk="));
        let st = s.state();
        assert_eq!(st.frame.as_ref().map(|f| (f.seq, f.ack_id)), Some((2, 2)));
        assert_eq!(st.unacked, vec![1]); // the replaced frame is acked with the next take
        assert_eq!(st.frame.as_ref().unwrap().meta.device_width, 800.0);
    }
}
