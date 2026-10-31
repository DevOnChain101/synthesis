//! `synthesis-desktop`: the desktop app. First run shows onboarding; `--screen workspace|settings`
//! opens that screen directly (development and screenshots).

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_desktop::credentials::InMemoryStore;
use synthesis_desktop::ui::{self, Nav, Screen};
use synthesis_desktop::{settings, theme};

fn main() {
    let env = Env::from_process();
    let (home, project) = match (synthesis_app::homes(&env), synthesis_app::launch_dir()) {
        (Ok(h), Ok(p)) => (h.synthesis, p),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("synthesis-desktop: {e}");
            std::process::exit(2);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (screen, nav) = match args
        .iter()
        .position(|a| a == "--screen")
        .and_then(|i| args.get(i + 1))
    {
        Some(s) if s == "workspace" => (Screen::Main, Nav::Session),
        Some(s) if s == "settings" => (Screen::Main, Nav::Configuration),
        _ => (Screen::Onboarding, Nav::Session),
    };
    gpui_platform::application()
        .with_assets(gpui_kit_assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            ui::register_fonts(cx);
            theme::apply(settings::load(&home).font.family(), cx);
            ui::open(
                screen,
                nav,
                home,
                project,
                env,
                Arc::new(InMemoryStore::default()),
                cx,
            );
            cx.activate(true);
        });
}
