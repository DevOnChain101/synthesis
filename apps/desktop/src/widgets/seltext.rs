//! A line of chat text that takes part in the window's text selection (drag, double and triple click, Cmd/Ctrl+C
//! through `TuiView`), with the syntax colours of `syntax::styled`. `gpui_base::SelectableText` has no
//! highlights, so this is the same element with them. The text it registers is exactly the text it paints
//! (already redacted and sanitized), so a copy is what the person sees.

use std::ops::Range;

use gpui::{
    App, BorderStyle, Bounds, Corners, Edges, Element, ElementId, GlobalElementId, HighlightStyle,
    Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Point,
    SharedString, StyledText, Window, transparent_black,
};
use gpui_base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};

pub struct SelText {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
}

impl SelText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            styled: StyledText::new(text.clone()),
            text,
        }
    }

    pub fn highlights(mut self, h: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        self.styled = self.styled.with_highlights(h);
        self
    }
}

/// The boxes a selection from `start` to `end` fills: the rest of the first line, the full lines between, the
/// start of the last line.
fn quads(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    lh: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(start, Point::new(end.x, end.y + lh))];
    }
    let mut v = vec![Bounds::from_corners(
        start,
        Point::new(bounds.right(), start.y + lh),
    )];
    if end.y > start.y + lh {
        v.push(Bounds::from_corners(
            Point::new(bounds.left(), start.y + lh),
            Point::new(bounds.right(), end.y),
        ));
    }
    v.push(Bounds::from_corners(
        Point::new(bounds.left(), end.y),
        Point::new(end.x, end.y + lh),
    ));
    v
}

impl IntoElement for SelText {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelText {
    type RequestLayoutState = TextSelectionHandle;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        gid: Option<&GlobalElementId>,
        iid: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let handle = window.with_element_state(
            gid.expect("SelText has an id"),
            |h: Option<TextSelectionHandle>, _| {
                let h = h.unwrap_or_else(|| TextSelectionHandle::new(self.text.clone(), cx));
                (h.clone(), h)
            },
        );
        let (id, ()) = self.styled.request_layout(gid, iid, window, cx);
        (id, handle)
    }

    fn prepaint(
        &mut self,
        gid: Option<&GlobalElementId>,
        iid: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled.prepaint(gid, iid, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let reg = TextSelectionRegistration::new(hitbox.clone(), bounds)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(handle, window, cx);
        handle.register(reg, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        gid: Option<&GlobalElementId>,
        iid: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        handle: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let before = gpui_base::TextSelection::selected_text(window, cx);
        let projection = handle.update_runs(
            &[TextSelectionRun::new(
                self.text.clone(),
                layout.clone(),
                bounds,
            )],
            cx,
        );
        if before != gpui_base::TextSelection::selected_text(window, cx) {
            window.refresh();
        }
        let color = gpui_component::Theme::global(cx).selection;
        for r in projection.ranges().iter().flatten() {
            let (Some(a), Some(b)) = (
                layout.position_for_index(r.start),
                layout.position_for_index(r.end),
            ) else {
                continue;
            };
            for q in quads(a, b, layout.bounds(), layout.line_height()) {
                window.paint_quad(PaintQuad {
                    bounds: q,
                    background: color.into(),
                    corner_radii: Corners::default(),
                    border_widths: Edges::default(),
                    border_color: transparent_black(),
                    border_style: BorderStyle::default(),
                });
            }
        }
        self.styled
            .paint(gid, iid, bounds, &mut (), &mut (), window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    #[test]
    fn a_selection_fills_first_middle_and_last_lines() {
        let b = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(60.)));
        let lh = px(20.);
        assert_eq!(
            quads(point(px(10.), px(0.)), point(px(50.), px(0.)), b, lh).len(),
            1
        );
        assert_eq!(
            quads(point(px(10.), px(0.)), point(px(50.), px(20.)), b, lh).len(),
            2
        );
        let q = quads(point(px(10.), px(0.)), point(px(50.), px(40.)), b, lh);
        assert_eq!(q.len(), 3);
        assert_eq!(q[1].size.width, px(100.));
    }
}
