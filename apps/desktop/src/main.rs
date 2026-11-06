//! `synthesis-desktop`: the native desktop app (docs/specs/desktop.md).

fn main() {
    gpui_platform::application()
        .with_assets(synthesis_desktop::brand::Assets)
        .run(|cx| {
            gpui_component::init(cx);
            synthesis_desktop::ui::register_fonts(cx);
            synthesis_desktop::theme::apply("JetBrains Mono", cx);
            synthesis_desktop::ui::open(cx);
            cx.activate(true);
        });
}
