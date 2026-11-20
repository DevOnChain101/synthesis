//! The agent's mouse (`browser_mouse`): move, click, double/right click, drag and wheel as CDP
//! `Input.dispatchMouseEvent` sequences. Coordinates come from a model: they are checked against the viewport,
//! the path is cut into a bounded number of steps and nothing here navigates (a navigation a click causes meets
//! the same `UrlPolicy` interception as `click`).

use std::time::Duration;

use serde_json::{Value, json};

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
            v.push(ev("mousePressed", a, "left", 1, 1));
            v.extend(glide(a, b, true));
            v.push(ev("mouseReleased", b, "left", 0, 1));
            v
        }
        Plan::Wheel((x, y), dx, dy) => {
            vec![json!({"type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy})]
        }
    }
}

impl BrowserRuntime {
    /// Like `click`'s aim: the element's box centre after scrolling it into view, kept inside the viewport.
    async fn ref_point(&self, tab: &Tab, r: &str, vp: (u32, u32)) -> Result<P> {
        let oid = self.object_for(tab, r).await?;
        let f = "function(){this.scrollIntoView({block:'center',inline:'center'});const r=this.getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2,r.width*r.height]}";
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
        let top = |n: u32| f64::from(n.max(1)) - 1.0;
        Ok((x.clamp(0.0, top(vp.0)), y.clamp(0.0, top(vp.1))))
    }

    async fn point(&self, tab: &Tab, t: &MouseTarget, vp: (u32, u32)) -> Result<P> {
        match t {
            MouseTarget::Point(x, y) => check_point((*x, *y), vp),
            MouseTarget::Ref(r) => self.ref_point(tab, r, vp).await,
        }
    }

    /// Runs one mouse action on the current tab and returns where the pointer ended.
    pub async fn mouse(&self, op: &MouseOp) -> Result<Spot> {
        let tab = self.current()?;
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
                let a = self.point(&tab, a, vp).await?;
                Plan::Drag(a, self.point(&tab, b, vp).await?)
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
        for e in events(plan, from) {
            let moved = e["type"] == "mouseMoved";
            if let Err(err) = self.call(&tab, "Input.dispatchMouseEvent", e).await {
                if matches!(plan, Plan::Drag(..)) {
                    // never leave the button held
                    let up = ev("mouseReleased", end, "left", 0, 1);
                    let _ = self.call(&tab, "Input.dispatchMouseEvent", up).await;
                }
                return Err(err);
            }
            if moved {
                tokio::time::sleep(STEP_PAUSE).await;
            }
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
}
