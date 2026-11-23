//! Development only (feature `shot`): `synthesis-desktop --shot out.png --view sa-chat [--size WxH] [--chat cards|off]`
//! draws the conversation offscreen on a fixed scene (one person line, a tool row, a reply, and a proposal card
//! the host forced because the conductor had read session text). Nothing is read or sent. `main.rs` calls
//! `parse` and `render` like the other shots.

use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};

use gpui::{
    AppContext as _, Context, Entity, HeadlessAppContext, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px, size,
};
use serde_json::{Value, json};
use synthesis_app::conductor::{
    ConductorHost, ConductorReads, Confirm, HostConfig, ItemRow, OutcomeRow, ProposalExecutor,
    SessionReport, SessionRow,
};
use synthesis_app::host_tools::HostTools as _;
use synthesis_config::Env;
use synthesis_view::convo::{Item, ToolState};

use super::glue::{DesktopEffects, Live, NoPort, OutcomeWorker};
use super::model::Gate;
use super::{ConductorChat, conversation};
use crate::superagent::tokens::{self as t, hsla};

pub struct Args {
    path: PathBuf,
    w: f32,
    h: f32,
    off: bool,
}

pub fn parse(args: &[String]) -> Option<Args> {
    let val = |f: &str| {
        args.iter()
            .position(|a| a == f)
            .and_then(|i| args.get(i + 1))
    };
    if val("--view").map(String::as_str) != Some("sa-chat") {
        return None;
    }
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((900., 820.));
    Some(Args {
        path: PathBuf::from(val("--shot")?),
        w,
        h,
        off: val("--chat").map(String::as_str) == Some("off"),
    })
}

struct Reads;

impl ConductorReads for Reads {
    fn sessions(&self) -> Vec<SessionRow> {
        vec![SessionRow {
            handle: "s1".into(),
            sid: "0123456789abcdef".into(),
            title: "auth tests".into(),
            state: "idle".into(),
            model: "fake/model".into(),
            mode: "default".into(),
            cwd_label: "auth".into(),
            spent_usd: 0.0,
            task_summary: String::new(),
            live: true,
        }]
    }
    fn session_report(&self, _: &str) -> Option<SessionReport> {
        None
    }
    fn outcomes(&self, _: Option<&str>) -> Vec<OutcomeRow> {
        vec![]
    }
    fn outcome(&self, _: &str) -> Option<OutcomeRow> {
        None
    }
    fn item(&self, _: &str) -> Option<ItemRow> {
        None
    }
    fn report(&self, _: &str) -> Option<String> {
        None
    }
}

fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
    let mut f = std::pin::pin!(f);
    match f
        .as_mut()
        .poll(&mut TaskCx::from_waker(futures::task::noop_waker_ref()))
    {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("not ready"),
    }
}

struct Scene(Entity<ConductorChat>);

impl Render for Scene {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(hsla(t::BG))
            .pt(px(24.))
            .child(conversation(&self.0))
    }
}

type Err = Box<dyn std::error::Error + Send + Sync>;

pub fn render(a: Args) -> Result<(), Err> {
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
    let home = std::env::temp_dir();
    let win = cx.open_window(size(px(a.w), px(a.h)), |window, cx| {
        let host = Arc::new(ConductorHost::new(
            HostConfig {
                max_sessions: 6,
                confirm: Confirm::All,
                home: home.clone(),
                synt_home: home.join(".synt-shot"),
                models: vec![],
                secrets: vec![],
            },
            Arc::new(Reads),
        ));
        let fx = Arc::new(DesktopEffects {
            port: Arc::new(NoPort),
            outcomes: Arc::new(OutcomeWorker::start(home.join(".synt-shot"))),
        });
        let exec = Arc::new(ProposalExecutor::new(host.clone(), fx));
        host.person_turn("tell the auth one to rerun the tests");
        let _ = ready(host.call("conductor_list_sessions", json!({})));
        let _ = ready(host.call(
            "conductor_propose_assignment",
            json!({"target": {"session": "s1"}, "text": "Rerun the auth tests and report which ones fail.", "basis": "described"}),
        ));
        let chat = cx.new(|cx| {
            let mut c = ConductorChat::new(
                Env::from_pairs([("HOME", "/nonexistent")]),
                PathBuf::from("/"),
                None,
                window,
                cx,
            );
            c.gate = if a.off { Gate::Off } else { Gate::Ready };
            c.live = Some(Live::for_test(host, exec));
            c.lines.push("Tell the auth session to rerun its tests.".into());
            c.convo.items = vec![
                Item::User(String::new()),
                Item::Tool {
                    id: "t1".into(),
                    name: "conductor_list_sessions".into(),
                    summary: String::new(),
                    state: ToolState::Ok,
                    preview: String::new(),
                    output: String::new(),
                    duration_ms: 0,
                    file: None,
                    diff: None,
                    input: Box::new(Value::Null),
                    status: None,
                    exit_code: None,
                },
                Item::Assistant {
                    text: "Two sessions could match. **s1 \"auth tests\"** is the closest, so I drafted a message for it. Nothing is sent until you press **Send**.".into(),
                    streaming: false, tainted: false
                },
            ];
            c.refresh_cards();
            for s in &mut c.cards {
                s.anchor = 3;
            }
            c
        });
        cx.new(|_| Scene(chat))
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(&a.path)?;
    eprintln!(
        "wrote {} ({}x{})",
        a.path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
