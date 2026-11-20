//! Stacked tab tiles of a Super Agent session (spec 5.2, D7): at most `STACK_MAX` tiles, the focused one in
//! front. A focus change plays a `SHUFFLE_SECS` deck shuffle (smoothstep); with reduce-motion it switches at
//! once. Animation frames are requested only while a shuffle runs. Not mounted yet (after P5).

use std::time::Instant;

use gpui::{IntoElement, ParentElement as _, Styled as _, Window, div, px};
use synthesis_cli_ui_render::theme as t;

use crate::theme::hsla;

pub const STACK_MAX: usize = 3;
pub const SHUFFLE_SECS: f32 = 0.34;
const TILE_W: f32 = 24.;
const TILE_H: f32 = 20.;
/// How far each tile behind sticks out to the right, and how far the swapping tiles swing sideways.
const PEEK: f32 = 4.;
const SWING: f32 = 9.;

/// Smoothstep easing, clamped to 0..1.
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// One tile to draw: which tab, its (fractional, mid-shuffle) depth, and its sideways swing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub tab: usize,
    pub depth: f32,
    pub swing: f32,
}

pub struct TabTiles {
    count: usize,
    focus: usize,
    from: usize,
    started: Option<Instant>,
    reduce_motion: bool,
}

impl TabTiles {
    pub fn new(count: usize, reduce_motion: bool) -> Self {
        Self {
            count,
            focus: 0,
            from: 0,
            started: None,
            reduce_motion,
        }
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Focuses `tab` (ignored when out of range). Starts a shuffle unless reduce-motion is on.
    pub fn set_focus(&mut self, tab: usize, now: Instant) {
        if tab >= self.count || tab == self.focus {
            return;
        }
        self.from = self.focus;
        self.focus = tab;
        self.started = (!self.reduce_motion).then_some(now);
    }

    fn progress(&self, now: Instant) -> Option<f32> {
        let p = now.saturating_duration_since(self.started?).as_secs_f32() / SHUFFLE_SECS;
        (p < 1.).then_some(p)
    }

    /// True while a shuffle runs: the only time a frame must be requested.
    pub fn animating(&self, now: Instant) -> bool {
        self.progress(now).is_some()
    }

    /// Depth of `tab` behind `focus`: the tabs after the focused one in cyclic order.
    fn depth_in(&self, tab: usize, focus: usize) -> usize {
        (tab + self.count - focus) % self.count
    }

    /// The tiles to draw, back to front (the front tile last). At most `STACK_MAX`.
    pub fn slots(&self, now: Instant) -> Vec<Slot> {
        let p = self.progress(now);
        let k = p.map_or(1., smoothstep);
        let mut out: Vec<Slot> = (0..self.count)
            .filter_map(|tab| {
                let to = self.depth_in(tab, self.focus) as f32;
                let from = if p.is_some() {
                    self.depth_in(tab, self.from) as f32
                } else {
                    to
                };
                let depth = from + (to - from) * k;
                // Tiles that are in view before or after the change.
                if from.min(to) >= STACK_MAX as f32 {
                    return None;
                }
                // The tiles that change place swing out and back (sin 0..pi); the rest stay put.
                let swing = if from != to {
                    (std::f32::consts::PI * k).sin() * SWING
                } else {
                    0.
                };
                Some(Slot { tab, depth, swing })
            })
            .collect();
        // Tiles sliding out of view (depth past the stack) are dropped once fully gone.
        out.retain(|s| s.depth < STACK_MAX as f32 || p.is_some());
        out.truncate(STACK_MAX + usize::from(p.is_some()));
        out.sort_by(|a, b| b.depth.total_cmp(&a.depth));
        out
    }
}

/// Draws the stack and, only while a shuffle runs, asks for the next frame.
pub fn tab_tiles(deck: &TabTiles, now: Instant, window: &mut Window) -> impl IntoElement {
    if deck.animating(now) {
        window.request_animation_frame();
    }
    let slots = deck.slots(now);
    let mut row = div()
        .relative()
        .w(px(TILE_W + PEEK * (STACK_MAX - 1) as f32 + SWING))
        .h(px(TILE_H));
    for s in slots {
        let shade = crate::theme::mix(
            t::ELEVATED,
            t::SURFACE,
            (s.depth / STACK_MAX as f32).min(1.),
        );
        row = row.child(
            div()
                .absolute()
                .left(px(s.depth * PEEK + s.swing))
                .top(px(0.))
                .w(px(TILE_W - s.depth * 2.))
                .h(px(TILE_H - s.depth * 2.))
                .rounded(px(5.))
                .border_1()
                .border_color(hsla(t::BORDER))
                .bg(hsla(shade)),
        );
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn later(t0: Instant, secs: f32) -> Instant {
        t0 + Duration::from_secs_f32(secs)
    }

    #[test]
    fn easing_curve() {
        assert_eq!(smoothstep(0.), 0.);
        assert_eq!(smoothstep(1.), 1.);
        assert_eq!(smoothstep(0.5), 0.5);
        assert_eq!((smoothstep(-1.), smoothstep(2.)), (0., 1.));
        let mut prev = 0.;
        for i in 1..=100 {
            let v = smoothstep(i as f32 / 100.);
            assert!(v >= prev);
            prev = v;
        }
    }

    #[test]
    fn at_most_three_tiles() {
        let t0 = Instant::now();
        let mut d = TabTiles::new(8, false);
        assert_eq!(d.slots(t0).len(), STACK_MAX);
        d.set_focus(5, t0);
        for s in [0., 0.1, 0.2, 0.33, 0.5] {
            assert!(d.slots(later(t0, s)).len() <= STACK_MAX + 1);
        }
        assert_eq!(d.slots(later(t0, 0.5)).len(), STACK_MAX);
        assert_eq!(TabTiles::new(2, false).slots(t0).len(), 2);
    }

    #[test]
    fn reduce_motion_switches_at_once() {
        let t0 = Instant::now();
        let mut d = TabTiles::new(4, true);
        d.set_focus(2, t0);
        assert!(!d.animating(t0));
        let front = d.slots(t0).pop().unwrap();
        assert_eq!((front.tab, front.depth, front.swing), (2, 0., 0.));
    }

    #[test]
    fn frames_only_while_shuffling() {
        let t0 = Instant::now();
        let mut d = TabTiles::new(4, false);
        assert!(!d.animating(t0), "idle");
        d.set_focus(1, t0);
        assert!(d.animating(later(t0, 0.1)));
        assert!(!d.animating(later(t0, 0.34)));
        assert!(!d.animating(later(t0, 5.)));
        d.set_focus(9, t0); // out of range: no change, no animation
        assert_eq!(d.focus(), 1);
    }

    #[test]
    fn shuffle_ends_at_rest() {
        let t0 = Instant::now();
        let mut d = TabTiles::new(3, false);
        d.set_focus(1, t0);
        let end = d.slots(later(t0, 1.));
        assert!(end.iter().all(|s| s.swing == 0. && s.depth.fract() == 0.));
        assert_eq!(end.last().unwrap().tab, 1);
    }
}
