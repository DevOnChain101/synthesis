//! Development only (feature `shot`): `synthesis-desktop --shot out.png --view agent [--size WxH]
//! [--tab chat|diff|commands|verify|browser|files] [--sub NAME] [--dialog]` draws the Agent view into a
//! headless gpui context (real text shaping, Metal/wgpu renderer, no window server) and writes a PNG.
//! `--live SCRIPT --prompt TEXT` runs the real engine on a scripted model (build with
//! `--features test-provider`) in a throwaway home and folder and draws that session instead of the sample.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use synthesis_config::Env;

use super::{AgentView, Tab, sample};
use crate::engine::{Engine, UiMsg};
use crate::live::{Live, Submitted, now_ms};

pub struct Args {
    path: PathBuf,
    w: f32,
    h: f32,
    tab: Tab,
    sub: Option<String>,
    dialog: bool,
    live: Option<(PathBuf, String)>,
}

/// `None` unless `--shot` and `--view agent` are both present.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    if val("--view").map(String::as_str) != Some("agent") {
        return None;
    }
    let path = PathBuf::from(val("--shot")?);
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1400., 900.));
    let tab = val("--tab")
        .and_then(|t| {
            Tab::ALL
                .into_iter()
                .find(|x| x.label() == t || (*t == "files" && *x == Tab::Files))
        })
        .unwrap_or(Tab::Chat);
    Some(Args {
        path,
        w,
        h,
        tab,
        sub: val("--sub").cloned(),
        dialog: args.iter().any(|a| a == "--dialog"),
        live: val("--live").map(|s| {
            (
                PathBuf::from(s),
                val("--prompt").cloned().unwrap_or_else(|| "hello".into()),
            )
        }),
    })
}

type Err = Box<dyn std::error::Error + Send + Sync>;

/// Runs the prompt on the real engine against the scripted model until the task ended or a dialog
/// waits. The folder is a throwaway git-less directory; `{WS}` in the script names it.
fn run_live(script: &PathBuf, prompt: &str) -> Result<(Live, Arc<Engine>, PathBuf), Err> {
    let tmp = std::env::temp_dir().join(format!("synthesis-agent-shot-{}", std::process::id()));
    let (ws, home) = (tmp.join("ws"), tmp.join("home"));
    std::fs::create_dir_all(&ws)?;
    std::fs::create_dir_all(&home)?;
    let (ws, home) = (std::fs::canonicalize(&ws)?, std::fs::canonicalize(&home)?);
    let env = Env::from_pairs([
        ("HOME", home.to_string_lossy().into_owned()),
        ("SYNT_HOME", home.to_string_lossy().into_owned()),
        (
            "PATH",
            Env::from_process()
                .get("PATH")
                .unwrap_or_default()
                .to_string(),
        ),
    ]);
    let sp = tmp.join("script.json");
    std::fs::write(
        &sp,
        std::fs::read_to_string(script)?.replace("{WS}", &ws.to_string_lossy()),
    )?;
    let provider = format!("fake:{}", sp.display());
    let (engine, mut rx) = Engine::start(&ws, &env, None, Some(&provider), Some("off"))?;
    let engine = Arc::new(engine);
    let mut live = Live::new();
    live.attach(engine.clone(), None);
    let drain = |live: &mut Live, rx: &mut futures::channel::mpsc::UnboundedReceiver<UiMsg>| {
        while let Ok(m) = rx.try_recv() {
            live.on_msg(m, now_ms());
        }
    };
    std::thread::sleep(Duration::from_millis(50));
    drain(&mut live, &mut rx);
    if let Submitted::Send(t) = live.submit(prompt) {
        engine.send(t).map_err(|e| e.to_string())?;
    }
    let t0 = Instant::now();
    loop {
        drain(&mut live, &mut rx);
        let ended = live
            .view
            .convo
            .items
            .iter()
            .any(|i| matches!(i, synthesis_view::convo::Item::TaskEnd { .. }));
        if !live.dialogs.is_empty() || (!live.busy && ended) {
            break;
        }
        if t0.elapsed() > Duration::from_secs(30) {
            return Err("timed out waiting for the scripted task".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    drain(&mut live, &mut rx);
    live.cwd = engine.cwd.clone();
    Ok((live, engine, tmp))
}

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
    let fixture = a.live.as_ref().map(|(s, p)| run_live(s, p)).transpose()?;
    let (live, engine, tmp) = match fixture {
        Some((l, e, t)) => (l, Some(e), Some(t)),
        None if a.dialog => (sample::live_with_dialog(), None, None),
        None => (sample::live(), None, None),
    };
    let (tab, sub) = (a.tab, a.sub.clone());
    let win = cx.open_window(size(px(a.w), px(a.h)), |window, cx| {
        cx.new(|cx| {
            let mut v = AgentView::with_sessions(window, cx, vec![live], 0.);
            v.set_tab(tab, cx);
            if let Some(name) = &sub {
                v.pick_sub(name);
            }
            v
        })
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(&a.path)?;
    drop(cx);
    if let Some(e) = engine {
        crate::engine::close_shared(e);
    }
    if let Some(t) = tmp {
        let _ = std::fs::remove_dir_all(t);
    }
    eprintln!(
        "wrote {} ({}x{})",
        a.path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
