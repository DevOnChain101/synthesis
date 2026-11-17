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
/// A frame whose base64 `data` is longer than this is dropped (and acked) before it is copied: a hostile page
/// must not make us buffer and decode frames up to the 64 MiB transport cap at 30 per second.
const MAX_DATA: usize = 16 << 20;

/// Where the frame sits in the page, as Chrome reports it (CSS pixels). Values come from the page's
/// browser process and are sanitized: **0 means invalid or unknown** (missing, not finite, negative size,
/// or beyond 1e7), so a consumer must treat a 0 size as "no frame geometry" before dividing by it.
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

#[derive(Clone)]
pub struct Frame {
    /// Counts frames received from Chrome since launch; gaps mean frames the consumer was too slow for.
    pub seq: u64,
    pub jpeg: Vec<u8>,
    pub meta: FrameMeta,
}

/// Never prints the pixels (page content) into a log.
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("seq", &self.seq)
            .field("jpeg_len", &self.jpeg.len())
            .field("meta", &self.meta)
            .finish()
    }
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
    /// Bumped by every `activate`; a frame or ack of an older generation is never used.
    generation: u64,
    /// False between `activate` and Chrome's answer to `startScreencast`: frames seen then belong to the
    /// previous cast of the same tab (in flight when it was re-targeted) and are dropped.
    open: bool,
    closed: bool,
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

/// A finite number within `lo..=hi`, else 0 (unknown).
fn num(v: &serde_json::Value, k: &str, lo: f64, hi: f64) -> f64 {
    v[k].as_f64()
        .filter(|x| x.is_finite() && (lo..=hi).contains(x))
        .unwrap_or(0.0)
}

impl Slot {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn generation(&self) -> u64 {
        self.state().generation
    }

    /// Called by the CDP dispatcher for every `Page.screencastFrame`; keeps only the newest. Returns the
    /// (session, ack id) of a frame that was dropped but must still be acked so Chrome keeps sending.
    pub(crate) fn push(&self, e: &CdpEvent) -> Option<(String, i64)> {
        let mut st = self.state();
        if st.closed || st.active.is_none() || st.active != e.session_id {
            return None;
        }
        let (Some(ack_id), Some(data)) =
            (e.params["sessionId"].as_i64(), e.params["data"].as_str())
        else {
            return None;
        };
        if !st.open || data.len() > MAX_DATA {
            return st.active.clone().map(|s| (s, ack_id));
        }
        let m = &e.params["metadata"];
        let seq = self.received.fetch_add(1, Ordering::SeqCst) + 1;
        let offset = |k| num(m, k, -1e7, 1e7);
        let new = Raw {
            seq,
            ack_id,
            data: data.to_owned(),
            meta: FrameMeta {
                device_width: num(m, "deviceWidth", 0.0, 16384.0),
                device_height: num(m, "deviceHeight", 0.0, 16384.0),
                page_scale_factor: num(m, "pageScaleFactor", 0.0, 100.0),
                offset_top: offset("offsetTop"),
                scroll_x: offset("scrollOffsetX"),
                scroll_y: offset("scrollOffsetY"),
                timestamp: num(m, "timestamp", 0.0, 1e11),
                device_pixel_ratio: 1.0,
            },
        };
        if let Some(old) = st.frame.replace(new)
            && st.unacked.len() < MAX_UNACKED
        {
            st.unacked.push(old.ack_id);
        }
        drop(st);
        self.notify.notify_waiters();
        None
    }

    /// Re-targets the view to `session` (None = off) and starts a new generation that accepts no frame
    /// until `open` runs. Returns the session cast before, if any, and the new generation.
    pub(crate) fn activate(&self, session: Option<String>) -> (Option<String>, u64) {
        let mut st = self.state();
        st.frame = None;
        st.unacked.clear();
        st.generation += 1;
        st.open = false;
        (std::mem::replace(&mut st.active, session), st.generation)
    }

    /// Chrome answered `startScreencast` of generation `generation`: frames from now on are new ones.
    pub(crate) fn open(&self, generation: u64) {
        let mut st = self.state();
        if st.generation == generation {
            st.open = true;
        }
    }

    /// The runtime is closed or the transport is dead: waiters get `None`.
    pub(crate) fn close(&self) {
        let mut st = self.state();
        st.closed = true;
        st.frame = None;
        drop(st);
        self.notify.notify_waiters();
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
    /// `None` once the runtime is closed or the browser connection is gone (a view turned off just stops
    /// producing frames, so wrap the call in a timeout or select if that matters).
    pub async fn next(&self) -> Option<Frame> {
        loop {
            let wait = self
                .slot
                .state()
                .last_take
                .map(|t| (t + MIN_INTERVAL).saturating_duration_since(Instant::now()));
            if let Some(w) = wait.filter(|w| !w.is_zero()) {
                tokio::time::sleep(w).await;
            }
            // Armed before the state is read, so a push or close in between is not missed.
            let notified = self.slot.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let taken = {
                let mut st = self.slot.state();
                if st.closed {
                    return None;
                }
                let raw = st.frame.take();
                raw.map(|r| {
                    st.last_take = Some(Instant::now());
                    let mut ids = std::mem::take(&mut st.unacked);
                    ids.push(r.ack_id);
                    (r, ids, st.active.clone(), st.generation)
                })
            };
            let Some((raw, ids, session, generation)) = taken else {
                notified.await;
                continue;
            };
            if let (Some(cdp), Some(session)) = (self.cdp.upgrade(), session) {
                tokio::spawn(async move {
                    for id in ids {
                        // an ack of an older cast would acknowledge a frame of the new one
                        if cdp.screencast.generation() != generation {
                            return;
                        }
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
                return Some(Frame {
                    seq: raw.seq,
                    jpeg,
                    meta: raw.meta,
                });
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

    fn active(s: &Slot, session: &str) {
        let (_, g) = s.activate(Some(session.into()));
        s.open(g);
    }

    #[test]
    fn keeps_only_the_newest_and_ignores_other_sessions_and_inactive() {
        let s = Slot::default();
        assert!(s.push(&ev("A", 1, "aGk=")).is_none()); // not active yet
        assert!(s.state().frame.is_none());
        active(&s, "A");
        assert!(s.push(&ev("B", 9, "aGk=")).is_none());
        s.push(&ev("A", 1, "aGk="));
        s.push(&ev("A", 2, "aGk="));
        let st = s.state();
        assert_eq!(st.frame.as_ref().map(|f| (f.seq, f.ack_id)), Some((2, 2)));
        assert_eq!(st.unacked, vec![1]); // the replaced frame is acked with the next take
        assert_eq!(st.frame.as_ref().unwrap().meta.device_width, 800.0);
    }

    #[test]
    fn oversized_frames_are_dropped_before_the_copy_but_acked() {
        let s = Slot::default();
        active(&s, "A");
        let big = "A".repeat(MAX_DATA + 1);
        assert_eq!(s.push(&ev("A", 5, &big)), Some(("A".into(), 5)));
        assert!(s.state().frame.is_none());
        assert_eq!(s.received.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn frames_of_an_older_generation_are_dropped_and_acked() {
        let s = Slot::default();
        active(&s, "A");
        s.push(&ev("A", 1, "aGk="));
        let (_, g) = s.activate(Some("A".into())); // same tab, re-targeted
        assert!(s.state().frame.is_none());
        // in flight from the old cast, before Chrome answered the new start
        assert_eq!(s.push(&ev("A", 2, "aGk=")), Some(("A".into(), 2)));
        s.open(g - 1); // a stale open does nothing
        assert!(s.push(&ev("A", 3, "aGk=")).is_some());
        s.open(g);
        assert!(s.push(&ev("A", 4, "aGk=")).is_none());
        assert_eq!(s.state().frame.as_ref().map(|f| f.ack_id), Some(4));
        assert_eq!(s.generation(), g);
    }

    #[test]
    fn metadata_is_sanitized() {
        let s = Slot::default();
        active(&s, "A");
        let mut e = ev("A", 1, "aGk=");
        e.params["metadata"] = json!({"deviceWidth": -5, "deviceHeight": 1e9, "pageScaleFactor": 1,
            "offsetTop": 2e7, "scrollOffsetX": -3, "scrollOffsetY": "x", "timestamp": 1.7e9});
        s.push(&e);
        let m = s.state().frame.as_ref().unwrap().meta;
        assert_eq!((m.device_width, m.device_height), (0.0, 0.0));
        assert_eq!((m.offset_top, m.scroll_x, m.scroll_y), (0.0, -3.0, 0.0));
        assert_eq!((m.page_scale_factor, m.timestamp), (1.0, 1.7e9));
        let mut e = ev("A", 2, "aGk=");
        e.params["metadata"] = json!({"deviceWidth": 1280, "deviceHeight": 720});
        s.push(&e);
        let m = s.state().frame.as_ref().unwrap().meta;
        assert_eq!(
            (m.device_width, m.device_height, m.timestamp),
            (1280.0, 720.0, 0.0)
        );
    }

    #[test]
    fn debug_of_a_frame_has_no_pixels() {
        let f = Frame {
            seq: 3,
            jpeg: b"SECRETPIXELS".to_vec(),
            meta: FrameMeta::default(),
        };
        let d = format!("{f:?}");
        assert!(
            d.contains("jpeg_len: 12") && !d.contains("SECRET") && !d.contains("83, 69"),
            "{d}"
        );
    }

    #[tokio::test]
    async fn next_returns_none_once_closed_and_wakes_waiters() {
        let slot = Arc::new(Slot::default());
        let fs = FrameStream {
            slot: slot.clone(),
            cdp: Weak::new(),
        };
        let waiter = tokio::spawn({
            let fs = fs.clone();
            async move { fs.next().await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        slot.close();
        let r = tokio::time::timeout(Duration::from_secs(5), waiter).await;
        assert!(matches!(r, Ok(Ok(None))), "{r:?}");
        assert!(fs.next().await.is_none());
    }
}
