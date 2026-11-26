//! The context-window ring of the composer footer: how much of the model's window the session uses. The numbers
//! are the TUI's own (`status.context.numbers()`); this file is the pure part (fraction, level, tooltip) and the
//! ring itself. The home chat has no usage data and draws none.

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Hsla, InteractiveElement as _, IntoElement as _,
    ParentElement as _, PathBuilder, Pixels, StatefulInteractiveElement as _, Styled as _, Window,
    canvas, div, point, px,
};
use synthesis_view::toolview::Tone;

use super::{hsla, t};
use crate::tui_view::chat::tone_fg;
use crate::tui_view::snapshot::tok;

const SIZE: f32 = 18.;
const STROKE: f32 = 2.;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Level {
    Normal,
    Warn,
    Crit,
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Gauge {
    /// 0..=1 (clamped).
    pub frac: f32,
    pub level: Level,
    pub tip: String,
}

/// `None` while the window is unknown (nothing to show).
pub(crate) fn gauge(used: u64, window: u64) -> Option<Gauge> {
    if window == 0 {
        return None;
    }
    let pct = used as f64 * 100. / window as f64;
    let level = if pct >= 95. {
        Level::Crit
    } else if pct >= 80. {
        Level::Warn
    } else {
        Level::Normal
    };
    Some(Gauge {
        frac: (pct / 100.).clamp(0., 1.) as f32,
        level,
        tip: format!(
            "{}% of context used ({} / {} tokens)",
            pct.round() as u64,
            tok(used),
            tok(window)
        ),
    })
}

fn arc_colour(l: Level) -> Hsla {
    match l {
        Level::Normal => hsla(t::TEXT),
        Level::Warn => tone_fg(Tone::Warn),
        Level::Crit => tone_fg(Tone::Err),
    }
}

/// A polyline of the circle from the top, clockwise, `frac` of the way round.
fn arc(b: Bounds<Pixels>, frac: f32, colour: Hsla, window: &mut Window) {
    let r = (SIZE - STROKE) / 2.;
    let (cx, cy) = (
        f32::from(b.origin.x) + SIZE / 2.,
        f32::from(b.origin.y) + SIZE / 2.,
    );
    let n = ((64. * frac).ceil() as usize).max(2);
    let mut pb = PathBuilder::stroke(px(STROKE));
    for i in 0..=n {
        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * i as f32 / n as f32;
        let p = point(px(cx + r * a.cos()), px(cy + r * a.sin()));
        if i == 0 {
            pb.move_to(p);
        } else {
            pb.line_to(p);
        }
    }
    if let Ok(path) = pb.build() {
        window.paint_path(path, colour);
    }
}

/// The ring (18 px, stroke only): a hairline track and the arc, with the tooltip.
pub(crate) fn ring(g: &Gauge) -> AnyElement {
    let (frac, colour, tip) = (g.frac, arc_colour(g.level), g.tip.clone());
    div()
        .id("sa-chat-ctx")
        .debug_selector(|| "sa-chat-ctx".to_string())
        .flex_none()
        .size(px(SIZE))
        .tooltip(move |_, cx| -> gpui::AnyView { cx.new(|_| crate::ui::Tip(tip.clone())).into() })
        .child(
            canvas(
                |_, _, _| (),
                move |b, _, window: &mut Window, _: &mut App| {
                    arc(b, 1., t::alpha(t::TEXT_STRONG, 0.16), window);
                    if frac > 0. {
                        arc(b, frac, colour, window);
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_window_shows_nothing() {
        assert_eq!(gauge(5000, 0), None);
    }

    #[test]
    fn fraction_level_and_tooltip() {
        let g = gauge(84_000, 200_000).unwrap();
        assert_eq!((g.frac, g.level), (0.42, Level::Normal));
        assert_eq!(g.tip, "42% of context used (84k / 200k tokens)");
        assert_eq!(gauge(160_000, 200_000).unwrap().level, Level::Warn);
        assert_eq!(gauge(159_999, 200_000).unwrap().level, Level::Normal);
        assert_eq!(gauge(190_000, 200_000).unwrap().level, Level::Crit);
        // over the window: the ring is full, the words keep the truth
        let g = gauge(250_000, 200_000).unwrap();
        assert_eq!((g.frac, g.level), (1., Level::Crit));
        assert!(g.tip.starts_with("125%"));
        assert_eq!(gauge(0, 200_000).unwrap().frac, 0.);
    }
}
