//! Development only (feature `shot`): `synthesis-desktop --shot out.png --view settings [--size WxH]
//! [--search TEXT] [--confirm] [--scroll PX] [--tab NAME] [--opacity PCT]` draws the whole shell (title bar,
//! TUI sample) with the Settings popup over it, offscreen, on a throwaway `SYNT_HOME` (never `~/.synt`), and
//! writes a few user values so origins, reset links and the restart badge show. `--opacity` paints the window
//! over coloured stripes (the desktop behind a real window) so the translucency can be seen.

use std::{path::PathBuf, sync::Arc};

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use synthesis_config::{Env, ProjectTrust};

use super::SettingsView;
use super::model::{Cfg, Change, McpForm};
use crate::tui_view::{TuiView, layout::Hidden};

pub struct Args {
    path: PathBuf,
    w: f32,
    h: f32,
    search: String,
    confirm: bool,
    /// Scroll the panel down by this many pixels.
    scroll: f32,
    /// The tab to open (its label, e.g. `Memory`).
    tab: Option<String>,
    /// Window opacity in percent.
    opacity: Option<u8>,
}

/// `None` unless `--view settings` is given with `--shot`.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    if val("--view").map(String::as_str) != Some("settings") {
        return None;
    }
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1400., 900.));
    Some(Args {
        path: PathBuf::from(val("--shot")?),
        w,
        h,
        search: val("--search").cloned().unwrap_or_default(),
        confirm: args.iter().any(|a| a == "--confirm"),
        scroll: val("--scroll").and_then(|s| s.parse().ok()).unwrap_or(0.),
        tab: val("--tab").cloned(),
        opacity: val("--opacity").and_then(|s| s.parse().ok()),
    })
}

type Err = Box<dyn std::error::Error + Send + Sync>;

pub fn render(a: Args) -> Result<(), Err> {
    let tmp = std::env::temp_dir().join(format!("synthesis-settings-shot-{}", std::process::id()));
    let (home, ws) = (tmp.join("home"), tmp.join("ws"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&ws)?;
    let (home, ws) = (std::fs::canonicalize(&home)?, std::fs::canonicalize(&ws)?);
    let env = Env::from_pairs([
        ("HOME", home.to_string_lossy().into_owned()),
        ("SYNT_HOME", home.to_string_lossy().into_owned()),
        ("OMNIROUTE_API_KEY", "x".to_string()),
    ]);
    std::fs::write(
        home.join("synt.json"),
        r#"{"$schema":"SCHEMA","providers":{"omniroute":{"type":"openai","baseURL":"http://localhost:20128/v1","apiKey":"{env:OMNIROUTE_API_KEY}","models":["m"]},"other":{"type":"openai","baseURL":"http://localhost:1/v1","apiKey":"{env:OTHER_API_KEY}","models":["m"]}}}"#.replace("SCHEMA", synthesis_config::SCHEMA_V1),
    )?;
    std::fs::write(ws.join("AGENTS.md"), "rules")?;
    let cfg = Cfg {
        env,
        user_home: home.clone(),
        home: home.clone(),
        cwd: ws,
        managed: tmp.join("managed.json"),
        trust: ProjectTrust::Untrusted,
    };
    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(crate::brand::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::ui::register_fonts(cx);
        crate::theme::apply("JetBrains Mono", cx);
    });
    let (tab, opacity) = (a.tab.clone(), a.opacity);
    crate::theme::set_opacity(opacity.unwrap_or(100));
    let win = cx.open_window(size(px(a.w), px(a.h)), |window, cx| {
        let tui = cx.new(|_| TuiView::new(0., Hidden::default()));
        let shell = cx.new(|cx| crate::ui::Shell::around(window, cx, tui, Ok(cfg)));
        shell.update(cx, |s, cx| {
            s.go(crate::ui::View::Settings, window, cx);
            s.settings().update(cx, |v, cx| {
                v.seed(&a.search, a.confirm, a.scroll, tab.as_deref(), window, cx);
                if let Some(o) = opacity {
                    v.apply("synt.tui.opacity", Change::Text(o.to_string()), window, cx);
                }
            });
        });
        cx.new(|_| Behind {
            shell,
            stripes: opacity.is_some(),
        })
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(&a.path)?;
    drop(cx);
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!(
        "wrote {} ({}x{})",
        a.path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}

/// The shell, over coloured stripes when the opacity is being looked at.
struct Behind {
    shell: gpui::Entity<crate::ui::Shell>,
    stripes: bool,
}

impl gpui::Render for Behind {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{ParentElement as _, Styled as _, div};
        let mut d = div().relative().size_full().flex();
        if self.stripes {
            for c in [0xd94a4a, 0xe8c547, 0x3fa7d6, 0x59c47a, 0xb06ad9] {
                d = d.child(div().h_full().flex_1().bg(gpui::rgb(c)));
            }
        }
        d.child(div().absolute().inset_0().child(self.shell.clone()))
    }
}

impl SettingsView {
    /// Shot setup: a few user values, a server, a search text, an open confirmation.
    fn seed(
        &mut self,
        search: &str,
        confirm: bool,
        scroll: f32,
        tab: Option<&str>,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if search.is_empty() {
            for (k, c) in [
                ("synt.memory.enabled", Change::Bool(false)),
                ("synt.budget.max_turns", Change::Text("40".into())),
                ("synt.tui.panels.usage", Change::Bool(false)),
                ("synt.mcp.toolSearch", Change::Choice("on".into())),
            ] {
                self.apply(k, c, window, cx);
            }
            self.apply(
                "synt.images.retentionDays",
                Change::Text("9999".into()),
                window,
                cx,
            );
            if let Some(m) = &mut self.model {
                let _ = m.add_mcp(&McpForm {
                    name: "docs".into(),
                    transport: "http".into(),
                    url: "https://example.com/mcp".into(),
                    header: "Authorization: Bearer ${DOCS_TOKEN}".into(),
                    ..Default::default()
                });
            }
        } else {
            self.search
                .update(cx, |s, cx| s.set_value(search.to_string(), window, cx));
            if let Some(m) = &mut self.model {
                m.filter = search.into();
            }
        }
        if let Some(t) =
            tab.and_then(|t| super::TABS.into_iter().find(|s| super::tab_label(*s) == t))
        {
            self.tab = t;
        }
        self.scroll.set_offset(gpui::point(px(0.), px(-scroll)));
        if confirm {
            self.apply("synt.memory.reflection", Change::Bool(true), window, cx);
        }
    }
}
