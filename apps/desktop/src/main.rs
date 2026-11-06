//! `synthesis-desktop`: the native desktop app (docs/specs/desktop.md).

fn main() {
    #[cfg(feature = "shot")]
    if let Some(a) = synthesis_desktop::tui_view::shot::parse(&std::env::args().collect::<Vec<_>>())
    {
        if let Err(e) = synthesis_desktop::tui_view::shot::render(a) {
            eprintln!("synthesis-desktop: shot: {e}");
            std::process::exit(1);
        }
        return;
    }
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
