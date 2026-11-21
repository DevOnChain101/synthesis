//! Small element builders for the Super Agent screens, one per row of the mock's port table (DESIGN-NOTES
//! section 10.4). Dimensions are the mock's CSS; colours come from `tokens`. No state lives here: hover and press are
//! style rules, everything else (selection, focus, callbacks) is passed in. Each root has a `debug_selector`.

use std::rc::Rc;

use gpui::{
    AnyElement, App, BoxShadow, Div, Hsla, InteractiveElement as _, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, point,
    prelude::*, px,
};

use super::tokens::{self as t, hsla};
use crate::widgets::chrome::OnClick;

/// A text run in one of the type steps (`t::T1..T5`), weight and colour.
pub(super) fn text(step: (f32, f32), weight: gpui::FontWeight, c: u32) -> Div {
    div()
        .text_size(px(step.0))
        .line_height(px(step.1))
        .font_weight(weight)
        .text_color(hsla(c))
        .whitespace_nowrap()
}

/// `inset 0 1px 0 warm-top` plus the contact shadow: the raised look of buttons and plates.
pub(super) fn raised() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: t::warm_top(),
            offset: point(px(0.), px(1.)),
            blur_radius: px(0.),
            spread_radius: px(0.),
            inset: true,
        },
        BoxShadow {
            color: gpui::black().opacity(0.4),
            offset: point(px(0.), px(1.)),
            blur_radius: px(2.),
            spread_radius: px(0.),
            inset: false,
        },
    ]
}

/// The 2 px accent focus ring.
pub(super) fn ring() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: hsla(t::ACCENT),
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(2.),
        inset: false,
    }]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    Disabled,
}

/// h 32, px 16, pill, 12/500. Hover and pressed share the border fill (gpui has no transforms for the .97 press).
pub fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    focused: bool,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let (fill, hover_fill, fg) = match kind {
        ButtonKind::Primary => (t::primary_fill(), hsla(t::ELEVATED), t::TEXT_STRONG),
        ButtonKind::Secondary | ButtonKind::Disabled => {
            (hsla(t::ELEVATED), hsla(t::BORDER), t::TEXT_STRONG)
        }
        ButtonKind::Ghost => (Hsla::transparent_black(), t::hover(), t::TEXT),
    };
    let enabled = kind != ButtonKind::Disabled;
    let mut b = text(t::T2, t::W_MEDIUM, fg)
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .justify_center()
        .gap(px(t::S2))
        .h(px(32.))
        .px(px(t::S4))
        .rounded(px(t::R_PILL))
        .bg(fill)
        .child(label.into());
    if kind == ButtonKind::Primary {
        b = b.border_1().border_color(t::primary_rim());
    }
    if kind != ButtonKind::Ghost {
        b = b.shadow(raised());
    }
    if focused {
        b = b.shadow(ring());
    }
    if enabled {
        b = b
            .cursor_pointer()
            .hover(move |d| d.bg(hover_fill))
            .active(move |d| d.bg(hover_fill));
        if let Some(f) = on_click {
            b = b.on_click(move |e, w, cx| f(e, w, cx));
        }
    } else {
        b = b.opacity(0.4);
    }
    b
}

pub type OnSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Housing (r 12, p 3, well fill, stroke) holding segments (h 28, px 16, r 8); the active one is a raised plate.
pub fn segmented(
    id: &'static str,
    labels: &[&str],
    active: usize,
    on_select: Option<OnSelect>,
) -> Stateful<Div> {
    let mut housing = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .gap(px(2.))
        .p(px(3.))
        .rounded(px(t::R_PANEL))
        .bg(t::well())
        .border_1()
        .border_color(t::stroke());
    for (i, l) in labels.iter().enumerate() {
        let on = i == active;
        let mut s = text(
            t::T2,
            t::W_MEDIUM,
            if on { t::TEXT_STRONG } else { t::TEXT },
        )
        .id((id, i))
        .debug_selector(move || format!("{id}-seg-{i}"))
        .flex()
        .items_center()
        .h(px(28.))
        .px(px(t::S4))
        .rounded(px(t::R_CONTROL))
        .cursor_pointer()
        .child(SharedString::from(l.to_string()));
        s = if on {
            s.bg(hsla(t::ELEVATED)).shadow(raised())
        } else {
            s.hover(|d| d.text_color(hsla(t::TEXT_STRONG)))
        };
        if let Some(f) = on_select.clone() {
            s = s.on_click(move |_, w, cx| f(i, w, cx));
        }
        housing = housing.child(s);
    }
    housing
}

/// A title tab: h 32, r 8, 124 to 188 wide, the active one raised; the close box (20) shows when active or `hovered`.
pub fn tab_pill(
    id: &'static str,
    title: impl Into<SharedString>,
    active: bool,
    closable: bool,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let mut d = text(
        t::T2,
        t::W_MEDIUM,
        if active { t::TEXT_STRONG } else { t::TEXT },
    )
    .id(id)
    .debug_selector(move || id.to_string())
    .flex()
    .items_center()
    .flex_basis(px(188.))
    .flex_shrink(1.)
    .min_w(px(124.))
    .h(px(32.))
    .rounded(px(t::R_CONTROL))
    .cursor_pointer()
    .child(
        div()
            .flex_1()
            .min_w(px(0.))
            .px(px(t::S3))
            .truncate()
            .child(title.into()),
    );
    d = if active {
        d.bg(hsla(t::ELEVATED)).shadow(raised())
    } else {
        d.hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
    };
    if closable {
        d = d.child(
            div()
                .debug_selector(move || format!("{id}-x"))
                .flex_none()
                .size(px(20.))
                .mr(px(t::S2))
                .rounded(px(t::R_PILL))
                .flex()
                .items_center()
                .justify_center()
                .when(!active, |x| x.opacity(0.))
                .child("×"),
        );
    }
    if let Some(f) = on_click {
        d = d.on_click(move |e, w, cx| f(e, w, cx));
    }
    d
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Needs,
    Blocked,
    Verified,
    Failed,
    Draft,
    Idle,
    Stopped,
}

impl Status {
    /// The shape and the word: state never rests on hue alone.
    pub fn glyph_and_word(self) -> (&'static str, &'static str) {
        match self {
            Self::Running => ("◐", "Running"),
            Self::Needs => ("◆", "Needs you"),
            Self::Blocked => ("■", "Blocked"),
            Self::Verified => ("✓", "Verified"),
            Self::Failed => ("✕", "Failed"),
            Self::Draft => ("◇", "Draft"),
            Self::Idle => ("○", "Idle"),
            Self::Stopped => ("▪", "Stopped"),
        }
    }
    pub(super) fn colour(self) -> u32 {
        match self {
            Self::Running => t::ACCENT_LIGHT,
            Self::Needs | Self::Blocked => t::SOFT_BEIGE,
            Self::Verified => t::ACCENT,
            Self::Failed => t::TEXT_STRONG,
            Self::Draft | Self::Idle | Self::Stopped => t::TEXT,
        }
    }
}

/// 16 px glyph, gap 8, 12 px word; both in the state's colour.
pub fn status_glyph(id: &'static str, s: Status) -> Div {
    let (g, w) = s.glyph_and_word();
    div()
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .gap(px(t::S2))
        .child(
            text(t::T4, t::W_REGULAR, s.colour())
                .debug_selector(move || format!("{id}-glyph"))
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .child(g),
        )
        .child(text(t::T2, t::W_REGULAR, s.colour()).child(w))
}

/// A typographic key: h 18, min w 18, px 5, r 5, hover fill with a stroke.
pub fn keycap(id: &'static str, key: impl Into<SharedString>) -> Div {
    text(t::T1, t::W_MEDIUM, t::TEXT)
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(18.))
        .h(px(18.))
        .px(px(5.))
        .rounded(px(5.))
        .bg(t::hover())
        .border_1()
        .border_color(t::stroke())
        .line_height(px(1.))
        .child(key.into())
}

/// min h 52, p 8 x 16, r 8; title 13/500, meta 12; trailing content is passed in.
pub fn list_row(
    id: &'static str,
    title: impl Into<SharedString>,
    meta: impl Into<SharedString>,
    trailing: Option<AnyElement>,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let mut r = div()
        .id(id)
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .gap(px(t::S4))
        .min_h(px(52.))
        .px(px(t::S4))
        .py(px(t::S2))
        .rounded(px(t::R_CONTROL))
        .cursor_pointer()
        .hover(|d| d.bg(hsla(t::SURFACE)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(
                    text(t::T3, t::W_MEDIUM, t::TEXT_STRONG)
                        .truncate()
                        .child(title.into()),
                )
                .child(
                    text(t::T2, t::W_REGULAR, t::TEXT)
                        .truncate()
                        .child(meta.into()),
                ),
        )
        .children(trailing);
    if let Some(f) = on_click {
        r = r.on_click(move |e, w, cx| f(e, w, cx));
    }
    r
}

/// Title 15/600 (tracking `TRACK_HEAD` is not applied: gpui has no letter spacing), an optional count in beige,
/// px 16, pb 8.
pub fn section_header(
    id: &'static str,
    title: impl Into<SharedString>,
    count: Option<usize>,
) -> Div {
    div()
        .debug_selector(move || id.to_string())
        .flex()
        .items_baseline()
        .gap(px(t::S2))
        .px(px(t::S4))
        .pb(px(t::S2))
        .child(text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG).child(title.into()))
        .children(count.map(|n| text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE).child(n.to_string())))
}

/// The one card level: p 24, gap 16, r 12, surface fill, no border or shadow. `ask` swaps the fill for muted blue
/// and adds the 1 px soft-beige rim.
pub fn card(id: &'static str, ask: bool) -> Div {
    let c = div()
        .debug_selector(move || id.to_string())
        .flex()
        .flex_col()
        .gap(px(t::S4))
        .p(px(t::S5))
        .rounded(px(t::R_PANEL));
    if ask {
        c.bg(hsla(t::MUTED_BLUE))
            .border_1()
            .border_color(hsla(t::SOFT_BEIGE))
    } else {
        c.bg(hsla(t::SURFACE))
    }
}

/// Composer target chip (`Super-Agent`, `This session`): pill, px 12, py 4, 12/500 on 16, hover fill.
pub fn composer_chip(id: &'static str, label: impl Into<SharedString>) -> Div {
    text(t::T2, t::W_MEDIUM, t::TEXT_STRONG)
        .debug_selector(move || id.to_string())
        .px(px(t::S3))
        .py(px(t::S1))
        .rounded(px(t::R_PILL))
        .bg(t::hover())
        .child(label.into())
}

/// h 4 pill on elevated, filled with accent to `frac` (clamped to 0..1).
pub fn progress_bar(id: &'static str, frac: f32) -> Div {
    div()
        .debug_selector(move || id.to_string())
        .h(px(4.))
        .min_w(px(48.))
        .rounded(px(t::R_PILL))
        .bg(hsla(t::ELEVATED))
        .overflow_hidden()
        .child(
            div()
                .debug_selector(move || format!("{id}-fill"))
                .h_full()
                .w(gpui::relative(frac.clamp(0., 1.)))
                .rounded(px(t::R_PILL))
                .bg(hsla(t::ACCENT)),
        )
}

/// Pill on elevated: message, then an optional ghost action (Undo); p 8 8 8 16, gap 16, 12 px.
pub fn toast(
    id: &'static str,
    message: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
        .debug_selector(move || id.to_string())
        .flex()
        .items_center()
        .gap(px(t::S4))
        .py(px(t::S2))
        .pr(px(t::S2))
        .pl(px(t::S4))
        .rounded(px(t::R_PILL))
        .bg(hsla(t::ELEVATED))
        .shadow(vec![BoxShadow {
            color: gpui::black().opacity(0.45),
            offset: point(px(0.), px(18.)),
            blur_radius: px(60.),
            spread_radius: px(0.),
            inset: false,
        }])
        .child(message.into())
        .children(action)
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, Pixels, Render, TestAppContext, VisualTestContext, size};

    use super::*;

    /// Renders `build()` in a column 600 px wide whose children keep their own width.
    struct Host(Box<dyn Fn() -> AnyElement>);
    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .w(px(600.))
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .child((self.0)())
        }
    }

    fn open(
        cx: &mut TestAppContext,
        build: impl Fn() -> AnyElement + 'static,
    ) -> &mut VisualTestContext {
        let (_, v) = cx.add_window_view(|_, _| Host(Box::new(build)));
        v.simulate_resize(size(px(800.), px(600.)));
        v.run_until_parked();
        v
    }

    fn rect(v: &mut VisualTestContext, sel: &'static str) -> (f32, f32) {
        let b: Bounds<Pixels> = v
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("no element {sel}"));
        (f32::from(b.size.width), f32::from(b.size.height))
    }

    fn near(got: f32, want: f32, what: &str) {
        assert!((got - want).abs() <= 1., "{what}: {got} != {want}");
    }

    #[gpui::test]
    fn button_is_32_high_in_every_kind(cx: &mut TestAppContext) {
        let v = open(cx, || {
            div()
                .flex()
                .gap(px(8.))
                .child(button("b-pri", "Approve", ButtonKind::Primary, false, None))
                .child(button("b-sec", "Cancel", ButtonKind::Secondary, true, None))
                .child(button("b-gho", "More", ButtonKind::Ghost, false, None))
                .child(button("b-dis", "Send", ButtonKind::Disabled, false, None))
                .into_any_element()
        });
        for s in ["b-pri", "b-sec", "b-gho", "b-dis"] {
            near(rect(v, s).1, 32., s);
        }
    }

    #[gpui::test]
    fn segmented_control_dimensions(cx: &mut TestAppContext) {
        let v = open(cx, || {
            segmented("seg", &["Chat", "Changes", "Activity"], 1, None).into_any_element()
        });
        near(rect(v, "seg-seg-0").1, 28., "segment");
        near(
            rect(v, "seg").1,
            28. + 6. + 2.,
            "housing: segment + padding 3 + stroke 1",
        );
        let (w0, w1) = (rect(v, "seg-seg-0").0, rect(v, "seg-seg-1").0);
        assert!(w0 > 32. && w1 > w0, "px 16 around the label: {w0} {w1}");
    }

    #[gpui::test]
    fn tab_is_32_high_and_188_wide_shrinking_to_124(cx: &mut TestAppContext) {
        let v = open(cx, || {
            div()
                .w(px(600.))
                .flex()
                .child(tab_pill("tab-a", "A session", true, true, None))
                .into_any_element()
        });
        near(rect(v, "tab-a").0, 188., "tab width");
        near(rect(v, "tab-a").1, 32., "tab height");
        near(rect(v, "tab-a-x").0, 20., "close box");
        let v = open(cx, || {
            div()
                .w(px(250.))
                .flex()
                .child(tab_pill("tab-b", "One", false, true, None))
                .child(tab_pill("tab-c", "Two", false, true, None))
                .into_any_element()
        });
        near(rect(v, "tab-b").0, 125., "two tabs share 250");
        let v = open(cx, || {
            div()
                .w(px(200.))
                .flex()
                .child(tab_pill("tab-d", "One", false, false, None))
                .child(tab_pill("tab-e", "Two", false, false, None))
                .into_any_element()
        });
        near(rect(v, "tab-d").0, 124., "min width");
    }

    #[gpui::test]
    fn status_glyph_is_16_and_has_a_word_for_each_state(cx: &mut TestAppContext) {
        let all = [
            Status::Running,
            Status::Needs,
            Status::Blocked,
            Status::Verified,
            Status::Failed,
            Status::Draft,
            Status::Idle,
            Status::Stopped,
        ];
        let mut shapes: Vec<_> = all.iter().map(|s| s.glyph_and_word().0).collect();
        shapes.sort();
        shapes.dedup();
        assert_eq!(shapes.len(), 8, "every state has its own shape");
        let v = open(cx, || status_glyph("st", Status::Needs).into_any_element());
        near(rect(v, "st-glyph").0, 16., "glyph w");
        near(rect(v, "st-glyph").1, 16., "glyph h");
    }

    #[gpui::test]
    fn keycap_row_card_chip_bar_toast_dimensions(cx: &mut TestAppContext) {
        let v = open(cx, || {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .items_start()
                .child(keycap("kc", "N"))
                .child(
                    div()
                        .w(px(400.))
                        .child(list_row("row", "Title", "meta", None, None)),
                )
                .child(
                    div()
                        .w(px(400.))
                        .child(card("card", false).child(div().h(px(20.)))),
                )
                .child(composer_chip("chip", "Super-Agent"))
                .child(div().w(px(200.)).child(progress_bar("bar", 0.5)))
                .child(section_header("sh", "Waiting", Some(3)))
                .child(toast(
                    "toast",
                    "Archived",
                    Some(button("undo", "Undo", ButtonKind::Ghost, false, None).into_any_element()),
                ))
                .into_any_element()
        });
        let kc = rect(v, "kc");
        near(kc.1, 18., "keycap h");
        near(kc.0.max(18.), kc.0, "keycap min w 18");
        near(
            rect(v, "row").1,
            52.,
            "row min h (two lines 20 + 16 = 36 + 16 padding)",
        );
        near(rect(v, "card").1, 20. + 48., "card: content + p 24 twice");
        near(rect(v, "chip").1, 16. + 8., "chip: line 16 + py 4 twice");
        near(rect(v, "bar").1, 4., "bar h");
        near(rect(v, "bar-fill").0, 100., "bar fill at 50 % of 200");
        near(rect(v, "sh").1, 24. + 8., "section header: 15/24 + pb 8");
        near(
            rect(v, "toast").1,
            48.,
            "toast: ghost button 32 + py 8 twice",
        );
    }
}
