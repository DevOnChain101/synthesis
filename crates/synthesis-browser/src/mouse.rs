//! The agent's mouse (`browser_mouse`): move, click, double/right click, drag and wheel as CDP
//! `Input.dispatchMouseEvent` sequences. Coordinates come from a model: they are checked against the viewport,
//! the path is cut into a bounded number of steps and nothing here navigates (a navigation a click causes meets
//! the same `UrlPolicy` interception as `click`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::cdp::Cdp;
use crate::{BrowserError, BrowserRuntime, Result, Spot, Tab, lock};

/// Largest wheel delta of one call, per axis, in CSS pixels.
pub const MAX_WHEEL: f64 = 5000.0;
/// Most `mouseMoved` events of one glide or drag path, and the distance one step covers.
const MAX_STEPS: usize = 30;
const STEP_PX: f64 = 40.0;
const STEP_PAUSE: Duration = Duration::from_millis(8);

type P = (f64, f64);

/// Where a mouse action goes: a point in CSS pixels of the viewport, or an element (as `click`: scrolled into
/// view, its box centre).
#[derive(Clone, Debug, PartialEq)]
pub enum MouseTarget {
    Point(f64, f64),
    Ref(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MouseOp {
    Move(MouseTarget),
    Click(MouseTarget),
    DoubleClick(MouseTarget),
    RightClick(MouseTarget),
    Drag(MouseTarget, MouseTarget),
    /// At the target, else where the pointer is, else the viewport centre.
    Wheel(Option<MouseTarget>, f64, f64),
}

/// An op with its points resolved and checked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Plan {
    Move(P),
    Click { at: P, right: bool, double: bool },
    Drag(P, P),
    Wheel(P, f64, f64),
}

/// A point of the viewport `vp`: finite and inside it.
fn check_point((x, y): P, vp: (u32, u32)) -> Result<P> {
    if x.is_finite()
        && y.is_finite()
        && x >= 0.0
        && y >= 0.0
        && x < f64::from(vp.0)
        && y < f64::from(vp.1)
    {
        return Ok((x, y));
    }
    Err(BrowserError::Page(format!(
        "the point is outside the viewport (0..{} x 0..{}) or not a number",
        vp.0, vp.1
    )))
}

fn ev(kind: &str, (x, y): P, button: &str, buttons: u8, clicks: u8) -> Value {
    json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons, "clickCount": clicks})
}

/// `mouseMoved` events from `from` to `to`, the last exactly at `to`; `held` is the button pressed on the way.
fn glide(from: P, to: P, held: bool) -> Vec<Value> {
    let d = (to.0 - from.0).hypot(to.1 - from.1);
    let n = ((d / STEP_PX).ceil() as usize).clamp(1, MAX_STEPS);
    (1..=n)
        .map(|i| {
            let f = i as f64 / n as f64;
            let p = (from.0 + (to.0 - from.0) * f, from.1 + (to.1 - from.1) * f);
            if held {
                ev("mouseMoved", p, "left", 1, 0)
            } else {
                ev("mouseMoved", p, "none", 0, 0)
            }
        })
        .collect()
}

/// Press at `a`, move to `b` with the button down, release.
fn drag_tail(a: P, b: P) -> Vec<Value> {
    let mut v = vec![ev("mousePressed", a, "left", 1, 1)];
    v.extend(glide(a, b, true));
    v.push(ev("mouseReleased", b, "left", 0, 1));
    v
}

/// The CDP events of `plan`, starting with the pointer at `from`.
fn events(plan: Plan, from: P) -> Vec<Value> {
    match plan {
        Plan::Move(to) => glide(from, to, false),
        Plan::Click { at, right, double } => {
            let (b, bits) = if right { ("right", 2) } else { ("left", 1) };
            let mut v = glide(from, at, false);
            for n in 1..=if double { 2 } else { 1 } {
                v.push(ev("mousePressed", at, b, bits, n));
                v.push(ev("mouseReleased", at, b, 0, n));
            }
            v
        }
        Plan::Drag(a, b) => {
            let mut v = glide(from, a, false);
            v.extend(drag_tail(a, b));
            v
        }
        Plan::Wheel((x, y), dx, dy) => {
            vec![json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})]
        }
    }
}

/// A press that was sent and not yet released: where the pointer last was, and the button.
pub(crate) type Held = Option<(P, &'static str)>;

/// Cancels a drag the call did not finish (the future was dropped by a cancel or a timeout): Escape (ends a native
/// drag), then the button comes up where the pointer last was. Does nothing when nothing is held.
struct ReleaseOnDrop {
    cdp: Arc<Cdp>,
    tab: Arc<Tab>,
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if lock(&self.tab.pressed).is_none() {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (cdp, tab) = (self.cdp.clone(), self.tab.clone());
            rt.spawn(async move {
                let _one = tab.mouse_lock.lock().await;
                release_held(&cdp, &tab).await;
            });
        }
    }
}

/// Best effort: Escape, then the release of the held button at the last point sent. The record is cleared only
/// once the release went through, so a call dropped (or a send that failed) in the middle leaves it for the next
/// attempt. `send(kind, params)` is the CDP call (`Input.dispatchKeyEvent` for "key", `Input.dispatchMouseEvent`).
async fn release_with<F, Fut>(pressed: &Mutex<Held>, mut send: F)
where
    F: FnMut(&'static str, Value) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let Some(((x, y), button)) = *lock(pressed) else {
        return;
    };
    for t in ["keyDown", "keyUp"] {
        let k = json!({"type": t, "key": "Escape", "code": "Escape", "windowsVirtualKeyCode": 27});
        let _ = send("key", k).await;
    }
    if send("mouse", ev("mouseReleased", (x, y), button, 0, 1))
        .await
        .is_ok()
    {
        *lock(pressed) = None;
    }
}

async fn release_held(cdp: &Cdp, tab: &Tab) {
    release_with(&tab.pressed, |kind, params| {
        let m = if kind == "key" {
            "Input.dispatchKeyEvent"
        } else {
            "Input.dispatchMouseEvent"
        };
        async move { cdp.send(Some(&tab.session), m, params).await.map(drop) }
    })
    .await;
}

impl BrowserRuntime {
    /// Like `click`'s aim: the element's box centre after scrolling it into view. A centre that is still outside
    /// the viewport (an element larger than it, or a page that did not scroll) is an error, not a clamped guess.
    async fn ref_point(&self, tab: &Tab, r: &str, vp: (u32, u32), scroll: bool) -> Result<P> {
        let oid = self.object_for(tab, r).await?;
        let f = if scroll {
            "function(){this.scrollIntoView({block:'center',inline:'center'});const r=this.getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2,r.width*r.height]}"
        } else {
            "function(){const r=this.getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2,r.width*r.height]}"
        };
        let v = self
            .call(
                tab,
                "Runtime.callFunctionOn",
                json!({"objectId": oid, "functionDeclaration": f, "returnByValue": true}),
            )
            .await?;
        let p = &v["result"]["value"];
        let (x, y) = (
            p[0].as_f64().unwrap_or(f64::NAN),
            p[1].as_f64().unwrap_or(f64::NAN),
        );
        if !(p[2].as_f64().unwrap_or(0.0) > 0.0 && x.is_finite() && y.is_finite()) {
            return Err(BrowserError::Page(format!("element {r} is not visible")));
        }
        check_point((x, y), vp)
            .map_err(|_| BrowserError::Page(format!("element {r} is not inside the viewport")))
    }

    async fn point(&self, tab: &Tab, t: &MouseTarget, vp: (u32, u32)) -> Result<P> {
        match t {
            MouseTarget::Point(x, y) => check_point((*x, *y), vp),
            MouseTarget::Ref(r) => self.ref_point(tab, r, vp, true).await,
        }
    }

    /// The tag (and id) of the element at a viewport point, from the DOM (no page script), for a label.
    pub async fn element_at(&self, x: f64, y: f64) -> Option<String> {
        let tab = self.current().ok()?;
        let n = self
            .call(
                &tab,
                "DOM.getNodeForLocation",
                json!({"x": x.round() as i64, "y": y.round() as i64}),
            )
            .await
            .ok()?;
        let d = self
            .call(
                &tab,
                "DOM.describeNode",
                json!({"backendNodeId": n["backendNodeId"]}),
            )
            .await
            .ok()?;
        let name = d["node"]["nodeName"].as_str()?.to_lowercase();
        let ok =
            |s: &str| s.len() <= 30 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !ok(&name) {
            return None;
        }
        let attrs = d["node"]["attributes"].as_array()?;
        let id = attrs
            .chunks(2)
            .find(|c| c[0] == "id")
            .and_then(|c| c.get(1)?.as_str())
            .filter(|i| {
                i.len() <= 30
                    && i.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
            });
        Some(id.map_or(name.clone(), |i| format!("{name}#{i}")))
    }

    /// One CDP mouse event, remembering a held button so a cancelled call can let it go.
    async fn send_mouse(&self, tab: &Tab, e: Value) -> Result<()> {
        let at = (
            e["x"].as_f64().unwrap_or(0.0),
            e["y"].as_f64().unwrap_or(0.0),
        );
        let moved = e["type"] == "mouseMoved";
        match e["type"].as_str() {
            // marked BEFORE it is sent: a call dropped while the press is on the wire still lets go
            Some("mousePressed") => {
                let b = if e["button"] == "right" {
                    "right"
                } else {
                    "left"
                };
                *lock(&tab.pressed) = Some((at, b));
            }
            Some("mouseMoved") => {
                if let Some(h) = lock(&tab.pressed).as_mut() {
                    h.0 = at;
                }
            }
            _ => {}
        }
        let released = e["type"] == "mouseReleased";
        self.call(tab, "Input.dispatchMouseEvent", e).await?;
        if released {
            *lock(&tab.pressed) = None;
        }
        if moved {
            tokio::time::sleep(STEP_PAUSE).await;
        }
        Ok(())
    }

    /// Runs one mouse action on the current tab and returns where the pointer ended. One at a time per tab.
    pub async fn mouse(&self, op: &MouseOp) -> Result<Spot> {
        let tab = self.current()?;
        let _one = tab.mouse_lock.lock().await;
        let _guard = ReleaseOnDrop {
            cdp: self.cdp.clone(),
            tab: tab.clone(),
        };
        // a button an earlier call left down (cancelled, or its release failed) comes up first
        release_held(&self.cdp, &tab).await;
        let vp = self.viewport(&tab).await;
        // the pointer's last place, inside the viewport (it may have been resized since)
        let last = *lock(&tab.mouse);
        let from = last
            .and_then(|p| check_point(p, vp).ok())
            .unwrap_or((f64::from(vp.0) / 2.0, f64::from(vp.1) / 2.0));
        let plan = match op {
            MouseOp::Move(t) => Plan::Move(self.point(&tab, t, vp).await?),
            MouseOp::Click(t) => Plan::Click {
                at: self.point(&tab, t, vp).await?,
                right: false,
                double: false,
            },
            MouseOp::DoubleClick(t) => Plan::Click {
                at: self.point(&tab, t, vp).await?,
                right: false,
                double: true,
            },
            MouseOp::RightClick(t) => Plan::Click {
                at: self.point(&tab, t, vp).await?,
                right: true,
                double: false,
            },
            MouseOp::Drag(a, b) => {
                // the end first: scrolling the start into view last keeps the start where it was measured
                let b = self.point(&tab, b, vp).await?;
                Plan::Drag(self.point(&tab, a, vp).await?, b)
            }
            MouseOp::Wheel(t, dx, dy) => {
                if !(dx.is_finite() && dy.is_finite()) {
                    return Err(BrowserError::Page("wheel deltas must be numbers".into()));
                }
                let at = match t {
                    Some(t) => self.point(&tab, t, vp).await?,
                    None => from,
                };
                Plan::Wheel(
                    at,
                    dx.clamp(-MAX_WHEEL, MAX_WHEEL),
                    dy.clamp(-MAX_WHEEL, MAX_WHEEL),
                )
            }
        };
        let end = match plan {
            Plan::Move(p) | Plan::Click { at: p, .. } | Plan::Drag(_, p) | Plan::Wheel(p, ..) => p,
        };
        let r = match (plan, op) {
            (Plan::Drag(a, b), MouseOp::Drag(start, stop)) => {
                self.drag(&tab, from, (a, b), (start, stop), vp).await
            }
            _ => {
                let mut r = Ok(());
                for e in events(plan, from) {
                    r = self.send_mouse(&tab, e).await;
                    if r.is_err() {
                        break;
                    }
                }
                r
            }
        };
        if let Err(e) = r {
            release_held(&self.cdp, &tab).await;
            return Err(e);
        }
        *lock(&tab.mouse) = Some(end);
        if !self.opts.headless {
            let click = !matches!(plan, Plan::Move(_) | Plan::Wheel(..));
            let js = format!(
                "window.__syntOverlay&&window.__syntOverlay.move({:.1},{:.1},{click})",
                end.0, end.1
            );
            let _ = self
                .call(&tab, "Runtime.evaluate", json!({"expression": js}))
                .await;
        }
        Ok(Spot {
            x: end.0,
            y: end.1,
            w: vp.0,
            h: vp.1,
        })
    }

    /// A drag: glide to the start, then measure element ends again without scrolling (the page may have moved
    /// during the glide, and scrolling the start into view may have moved the end), press, move with the button
    /// down, release.
    async fn drag(
        &self,
        tab: &Tab,
        from: P,
        (mut a, mut b): (P, P),
        (start, stop): (&MouseTarget, &MouseTarget),
        vp: (u32, u32),
    ) -> Result<()> {
        for e in glide(from, a, false) {
            self.send_mouse(tab, e).await?;
        }
        if let MouseTarget::Ref(r) = start {
            let now = self.ref_point(tab, r, vp, false).await?;
            if now != a {
                self.send_mouse(tab, ev("mouseMoved", now, "none", 0, 0))
                    .await?;
            }
            a = now;
        }
        if let MouseTarget::Ref(r) = stop {
            b = self.ref_point(tab, r, vp, false).await?;
        }
        for e in drag_tail(a, b) {
            self.send_mouse(tab, e).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(v: &[Value]) -> Vec<String> {
        v.iter()
            .map(|e| e["type"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn points_must_be_finite_and_inside_the_viewport() {
        let vp = (1280, 800);
        assert_eq!(check_point((0.0, 0.0), vp).unwrap(), (0.0, 0.0));
        assert!(check_point((1279.9, 799.9), vp).is_ok());
        for p in [
            (f64::NAN, 1.0),
            (1.0, f64::INFINITY),
            (-0.1, 5.0),
            (5.0, -1.0),
            (1280.0, 5.0),
            (5.0, 800.0),
            (1e300, 1e300),
        ] {
            assert!(check_point(p, vp).is_err(), "{p:?}");
        }
    }

    #[test]
    fn a_click_glides_then_presses_and_releases_at_the_point() {
        let v = events(
            Plan::Click {
                at: (100.0, 50.0),
                right: false,
                double: false,
            },
            (0.0, 0.0),
        );
        let k = kinds(&v);
        assert_eq!(&k[k.len() - 2..], ["mousePressed", "mouseReleased"]);
        assert!(k[..k.len() - 2].iter().all(|t| t == "mouseMoved"));
        assert_eq!(
            (v[0]["button"].clone(), v[0]["buttons"].clone()),
            (json!("none"), json!(0))
        );
        let (p, r) = (&v[k.len() - 2], &v[k.len() - 1]);
        assert_eq!(
            (p["x"].clone(), p["y"].clone(), p["clickCount"].clone()),
            (json!(100.0), json!(50.0), json!(1))
        );
        assert_eq!(
            (p["button"].clone(), p["buttons"].clone()),
            (json!("left"), json!(1))
        );
        assert_eq!(r["buttons"], 0);
    }

    #[test]
    fn double_and_right_click_counts_and_buttons() {
        let v = events(
            Plan::Click {
                at: (5.0, 5.0),
                right: false,
                double: true,
            },
            (5.0, 5.0),
        );
        assert_eq!(
            kinds(&v),
            [
                "mouseMoved",
                "mousePressed",
                "mouseReleased",
                "mousePressed",
                "mouseReleased"
            ]
        );
        assert_eq!(v[3]["clickCount"], 2);
        let v = events(
            Plan::Click {
                at: (5.0, 5.0),
                right: true,
                double: false,
            },
            (5.0, 5.0),
        );
        assert_eq!(
            (v[1]["button"].clone(), v[1]["buttons"].clone()),
            (json!("right"), json!(2))
        );
    }

    #[test]
    fn a_drag_holds_the_button_on_the_way_and_the_path_is_bounded() {
        let v = events(Plan::Drag((10.0, 10.0), (4000.0, 3000.0)), (10.0, 10.0));
        let k = kinds(&v);
        let press = k.iter().position(|t| t == "mousePressed").unwrap();
        assert_eq!(k.last().unwrap(), "mouseReleased");
        assert!(
            v[press + 1..v.len() - 1]
                .iter()
                .all(|e| e["buttons"] == 1 && e["type"] == "mouseMoved")
        );
        assert!(v.len() <= 2 * MAX_STEPS + 2, "{}", v.len());
        let last = &v[v.len() - 2];
        assert_eq!(
            (last["x"].clone(), last["y"].clone()),
            (json!(4000.0), json!(3000.0))
        );
    }

    #[test]
    fn a_move_without_distance_is_one_event_and_a_wheel_has_the_deltas() {
        assert_eq!(events(Plan::Move((7.0, 7.0)), (7.0, 7.0)).len(), 1);
        let v = events(Plan::Wheel((1.0, 2.0), 3.0, -4.0), (0.0, 0.0));
        assert_eq!(
            v,
            [json!({"type": "mouseWheel", "x": 1.0, "y": 2.0, "deltaX": 3.0, "deltaY": -4.0})]
        );
    }

    #[tokio::test]
    async fn the_held_record_is_cleared_only_after_the_release_went_through() {
        let held = |p: &Mutex<Held>| lock(p).is_some();
        let p: Mutex<Held> = Mutex::new(Some(((1.0, 2.0), "left")));
        // the release fails: still held, so the next attempt tries again
        release_with(&p, |k, _| async move {
            if k == "mouse" {
                Err(BrowserError::Closed)
            } else {
                Ok(())
            }
        })
        .await;
        assert!(held(&p));
        // the call is dropped while the release is on the wire: still held
        let r = tokio::time::timeout(
            Duration::from_millis(30),
            release_with(&p, |k, _| async move {
                if k == "mouse" {
                    std::future::pending::<()>().await;
                }
                Ok(())
            }),
        )
        .await;
        assert!(r.is_err() && held(&p));
        // it goes through: cleared, and the release is at the last point, the key first
        let sent = Mutex::new(vec![]);
        release_with(&p, |k, v| {
            lock(&sent).push((k, v["type"].as_str().unwrap().to_owned(), v["x"].clone()));
            async { Ok(()) }
        })
        .await;
        assert!(!held(&p));
        let sent = lock(&sent).clone();
        assert_eq!(sent[0].0, "key");
        assert_eq!(sent.last().unwrap().1, "mouseReleased");
        assert_eq!(sent.last().unwrap().2, json!(1.0));
    }
}
