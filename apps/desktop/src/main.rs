//! `synthesis-desktop`: the desktop app. First run shows onboarding; `--screen NAME` opens that
//! screen directly (development and screenshots): workspace, settings, inbox, activity, tools,
//! topology. With the `test-provider` feature: `--fake script.json`, `--send "text"`.

use std::sync::Arc;

use synthesis_config::Env;
use synthesis_desktop::credentials::InMemoryStore;
use synthesis_desktop::ui::{self, Launch, Nav, Screen};
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
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (screen, nav) = match arg("--screen").as_deref() {
        Some("workspace") => (Screen::Main, Nav::Session),
        Some("settings") => (Screen::Main, Nav::Configuration),
        Some("inbox") => (Screen::Main, Nav::Inbox),
        Some("activity") => (Screen::Main, Nav::Activity),
        Some("tools") => (Screen::Main, Nav::Tools),
        Some("topology") => (Screen::Main, Nav::Topology),
        _ => (Screen::Onboarding, Nav::Session),
    };
    let (fake, auto_send) = if cfg!(feature = "test-provider") {
        (arg("--fake").map(|f| format!("fake:{f}")), arg("--send"))
    } else {
        (None, None)
    };
    let launch = Launch {
        screen,
        nav,
        home: home.clone(),
        project,
        env,
        store: Arc::new(InMemoryStore::default()),
        fake,
        auto_send,
        side: arg("--side"),
        open: arg("--open"),
        shot: arg("--shot").map(|p| {
            let secs = args
                .iter()
                .position(|a| a == "--shot")
                .and_then(|i| args.get(i + 2))
                .and_then(|s| s.parse().ok())
                .unwrap_or(6);
            (std::path::PathBuf::from(p), secs)
        }),
    };
    gpui_platform::application()
        .with_assets(synthesis_desktop::brand::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            cx.bind_keys(ui::bindings());
            ui::register_fonts(cx);
            theme::apply(settings::load(&home).font.family(), cx);
            ui::open(launch, cx);
            cx.activate(true);
        });
}
