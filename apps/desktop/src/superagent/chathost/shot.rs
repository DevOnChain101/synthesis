//! Development only (feature `shot`): the session chat on a sample conversation, offscreen, to a PNG.
//! `cargo run -p synthesis-desktop --features shot --example chathost_shot -- DIR` writes `chathost.png` (a turn
//! with tool cards, a diff and a command block, and a permission ask above the composer) and `chathost-idle.png`.
//! The text is built here, never a real secret.

use std::{path::Path, sync::Arc};

use clap::Parser as _;
use gpui::{
    AppContext as _, Context, HeadlessAppContext, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px, size,
};
use serde_json::json;
use synthesis_view::convo::{Item, ToolState};

use super::ChatHost;
use crate::engine::Args;
use crate::live::{Dialog, DialogKind, Live};
use crate::superagent::tokens::{self as t, hsla};
use crate::tui_view::driver::LiveCfg;
use crate::tui_view::{Embed, TuiView};

struct Panel(gpui::Entity<ChatHost>);

impl Render for Panel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(hsla(t::BG)).child(self.0.clone())
    }
}

type Err = Box<dyn std::error::Error + Send + Sync>;

fn tool(
    id: &str,
    name: &str,
    summary: &str,
    input: serde_json::Value,
    diff: Option<serde_json::Value>,
    output: &str,
) -> Item {
    Item::Tool {
        id: id.into(),
        name: name.into(),
        summary: summary.into(),
        state: ToolState::Ok,
        preview: String::new(),
        output: output.into(),
        duration_ms: 340,
        file: None,
        diff,
        input: Box::new(input),
        status: None,
        exit_code: None,
    }
}

fn sample(idle: bool) -> Live {
    let mut l = Live::new();
    l.starting = false;
    l.busy = !idle;
    l.view.status.model = "anthropic/claude-sonnet-5-5".into();
    l.view.status.mode = "default".into();
    let diff = json!({"path": "crates/cli/src/status.rs", "added": 3, "removed": 1, "hunks": [{"lines": [
        {"tag": "-", "text": "    println!(\"{}\", report);"},
        {"tag": "+", "text": "    if args.json {"},
        {"tag": "+", "text": "        println!(\"{}\", serde_json::to_string(&report)?);"},
        {"tag": "+", "text": "    }"}]}]});
    l.view.convo.items = vec![
        Item::User("Add a --json flag to the status command and show me the change.".into()),
        Item::Assistant {
            text: "I will read the command first, then make the change.".into(),
            streaming: false,
            tainted: false,
        },
        tool(
            "t1",
            "Read",
            "status.rs",
            json!({"file_path": "crates/cli/src/status.rs"}),
            None,
            "fn run() {}",
        ),
        tool(
            "t2",
            "Edit",
            "status.rs",
            json!({"file_path": "crates/cli/src/status.rs", "old_string": "a", "new_string": "b"}),
            Some(diff),
            "ok",
        ),
        Item::Assistant {
            text: "The flag is in. Run the tests with:\n\n```bash\ncargo test -p cli status\n```\n"
                .into(),
            streaming: false,
            tainted: false,
        },
    ];
    if !idle {
        l.dialogs.push(Dialog {
            kind: DialogKind::Plan {
                id: "p1".into(),
                plan: "1. Run the status tests\n2. Report the result".into(),
            },
            options: vec![
                ("yes".into(), "Yes, go ahead".into()),
                ("no".into(), "No, keep planning".into()),
            ],
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
    }
    l
}

/// `idle`: the conversation alone; otherwise a turn is running and an ask waits.
pub fn render(path: &Path, idle: bool, (w, h): (f32, f32)) -> Result<(), Err> {
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
    let tmp = std::env::temp_dir().join("synthesis-chathost-shot");
    let cfg = LiveCfg {
        env: synthesis_config::Env::from_pairs([("HOME", tmp.to_str().unwrap_or("."))]),
        args: Args::try_parse_from(["synthesis-desktop"])?,
        project: tmp.clone(),
        granted: tmp.clone(),
        synthesis_home: tmp.join("home"),
        home: None,
        tz_min: 0,
        tui: Default::default(),
    };
    let win = cx.open_window(size(px(w), px(h)), move |window, cx| {
        let tui = cx.new(|cx| TuiView::live(window, cx, cfg, Default::default(), 0.));
        tui.update(cx, |t, _| {
            t.embed = Some(Embed {
                headless: true,
                ..Embed::default()
            });
            t.set_live(sample(idle));
        });
        let chat = cx.new(|cx| ChatHost::new(tui, window, cx));
        cx.new(|_| Panel(chat))
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(path)?;
    eprintln!(
        "wrote {} ({}x{})",
        path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
