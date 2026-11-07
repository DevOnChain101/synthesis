//! Development only (feature `shot`): `synthesis-desktop --shot out.png [--size WxH] [--hide a,b]` draws
//! the TUI view into a headless gpui context (real text shaping, Metal/wgpu renderer, no window server)
//! and writes the frame to a PNG, so no Screen Recording permission and no display are needed.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Parser as _;
use gpui::{AppContext as _, HeadlessAppContext, px, size};
use synthesis_config::Env;

use super::TuiView;
use super::driver::LiveCfg;
use super::layout::Hidden;
use crate::engine::{Choice, Engine, UiMsg};
use crate::live::{Live, Submitted, now_ms};

pub struct Args {
    path: PathBuf,
    /// `--tab chat|diff|terminal|browser|files`: the centre tab shown.
    tab: super::ChatTab,
    /// `--drawer left|right`: the slide-over column of a narrow window, open.
    drawer: Option<super::Side>,
    /// `--open FILE`: the file selected in the file explorer.
    open: Option<String>,
    w: f32,
    h: f32,
    hidden: Hidden,
    live: Option<LiveShot>,
}

/// `--live SCRIPT --prompt TEXT [--answer once|always|deny|none]`: the real engine on a scripted model
/// (needs a build with `--features test-provider`) runs the prompt in a throwaway home and folder,
/// and the window shows the resulting conversation.
pub struct LiveShot {
    script: PathBuf,
    prompt: String,
    answer: Option<Choice>,
    /// `--cmd TEXT`: typed after the task, so a command's output or picker is in the frame.
    cmd: Option<String>,
    /// `--earlier TEXT`: a first session of the same folder and home that ran this prompt and
    /// ended (what `/resume` lists).
    earlier: Option<String>,
}

/// `None` when `--shot` is absent.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let path = PathBuf::from(val("--shot")?);
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1400., 900.));
    let mut hidden = Hidden::default();
    for k in val("--hide").iter().flat_map(|s| s.split(',')) {
        if !hidden.hide(k) {
            eprintln!("synthesis-desktop: --hide: {k} is not a panel key");
        }
    }
    let live = val("--live").map(|script| LiveShot {
        script: PathBuf::from(script),
        prompt: val("--prompt").cloned().unwrap_or_else(|| "hello".into()),
        answer: match val("--answer").map(String::as_str) {
            Some("once") => Some(Choice::Once),
            Some("always") => Some(Choice::Always),
            Some("deny") => Some(Choice::Deny),
            _ => None,
        },
        cmd: val("--cmd").cloned(),
        earlier: val("--earlier").cloned(),
    });
    let tab = match val("--tab").map(String::as_str) {
        Some("diff") => super::ChatTab::Diff,
        Some("terminal") => super::ChatTab::Terminal,
        Some("browser") => super::ChatTab::Browser,
        Some("files") => super::ChatTab::Files,
        _ => super::ChatTab::Chat,
    };
    Some(Args {
        path,
        tab,
        drawer: match val("--drawer").map(String::as_str) {
            Some("left") => Some(super::Side::Left),
            Some("right") => Some(super::Side::Right),
            _ => None,
        },
        open: val("--open").cloned(),
        w,
        h,
        hidden,
        live,
    })
}

type Err = Box<dyn std::error::Error + Send + Sync>;

/// Runs the prompt on the real engine against the scripted model: until a dialog waits, or the
/// task ended. A dialog is answered when `--answer` says so.
fn run_live(l: &LiveShot) -> Result<(Live, LiveCfg, Arc<Engine>, PathBuf), Err> {
    let tmp = std::env::temp_dir().join(format!("synthesis-shot-{}", std::process::id()));
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
    // `{WS}` in the script is the throwaway folder (a Write needs an absolute path)
    let script = tmp.join("script.json");
    std::fs::write(
        &script,
        std::fs::read_to_string(&l.script)?.replace("{WS}", &ws.to_string_lossy()),
    )?;
    let provider = format!("fake:{}", script.display());
    if let Some(p) = &l.earlier {
        let (e, mut rx) = Engine::start(&ws, &env, None, Some(&provider), Some("off"))?;
        e.send(p.clone()).map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        loop {
            match rx.try_recv() {
                Ok(UiMsg::TaskDone(_)) => break,
                Ok(_) => {}
                Err(_) if t0.elapsed() > Duration::from_secs(30) => {
                    return Err("timed out waiting for the earlier session".into());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        crate::engine::close_shared(Arc::new(e));
    }
    let (engine, mut rx) = Engine::start(&ws, &env, None, Some(&provider), Some("off"))?;
    let engine = Arc::new(engine);
    let mut live = Live::new();
    live.show_thinking = true;
    live.attach(engine.clone(), None);
    let drain = |live: &mut Live, rx: &mut futures::channel::mpsc::UnboundedReceiver<UiMsg>| {
        while let Ok(m) = rx.try_recv() {
            live.on_msg(m, now_ms());
        }
    };
    std::thread::sleep(Duration::from_millis(50));
    drain(&mut live, &mut rx);
    if let Submitted::Send(t) = live.submit(&l.prompt) {
        engine.send(t).map_err(|e| e.to_string())?;
    }
    let t0 = Instant::now();
    loop {
        drain(&mut live, &mut rx);
        if !live.dialogs.is_empty() {
            match l.answer {
                Some(c) => {
                    let id = live.dialogs[0].id().to_string();
                    engine.answer(&id, c);
                    live.dialogs.clear();
                }
                None => break,
            }
        }
        if !live.busy
            && live
                .view
                .convo
                .items
                .iter()
                .any(|i| matches!(i, synthesis_view::convo::Item::TaskEnd { .. }))
        {
            break;
        }
        if t0.elapsed() > Duration::from_secs(30) {
            return Err("timed out waiting for the scripted task".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(50));
    drain(&mut live, &mut rx);
    let args = crate::engine::Args::try_parse_from(["synthesis-desktop"])?;
    let cfg = LiveCfg {
        granted: crate::engine::root_of(&ws),
        project: ws.clone(),
        synthesis_home: home.clone(),
        home: None,
        tz_min: 0,
        tui: Default::default(),
        args,
        env,
    };
    if let Ok((w, days)) = crate::tui_view::driver::read_store(&cfg, Some(&engine.root)) {
        live.view.workspaces = w;
        live.view.usage = synthesis_view::usage::UsageHistory { days };
        live.view.set_peers(&[]);
        live.ws_key = synthesis_view::workspaces::fold_dir(&engine.root);
    }
    if let Some(c) = &l.cmd {
        live.submit(c);
    }
    Ok((live, cfg, engine, tmp))
}

pub fn render(a: Args) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    let hidden = a.hidden;
    let fixture = a.live.as_ref().map(run_live).transpose()?;
    let (live, engine, tmp) = match fixture {
        Some((mut l, c, e, t)) => {
            prepare_tabs(&mut l, &e, &t, a.tab)?;
            (Some((l, c)), Some(e), Some(t))
        }
        None => (None, None, None),
    };
    let tui_slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<TuiView>>>> =
        Default::default();
    let slot = tui_slot.clone();
    let tab = a.tab;
    let win = cx.open_window(size(px(a.w), px(a.h)), |window, cx| {
        let tui = cx.new(|cx| match live {
            Some((l, c)) => {
                let mut v = TuiView::live(window, cx, c, hidden, 0.);
                v.set_live(l);
                v
            }
            None => TuiView::new(0., hidden),
        });
        tui.update(cx, |t, _| {
            t.ts.tab = tab;
            t.drawer = a.drawer;
        });
        *slot.borrow_mut() = Some(tui.clone());
        cx.new(|cx| crate::ui::Shell::around(window, cx, tui, Err("--shot".into())))
    })?;
    cx.run_until_parked();
    if let Some(tui) = tui_slot.borrow().clone() {
        if a.tab == super::ChatTab::Browser {
            // the saved screenshot loads off the thread
            for _ in 0..40 {
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if a.tab == super::ChatTab::Files {
            // the project tree is read off the thread: wait for it, then open the asked file
            for _ in 0..100 {
                cx.run_until_parked();
                if tui.read_with(&cx, |t, _| t.tree_loaded()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if let Some(f) = &a.open {
                tui.update(&mut cx, |t, cx| t.select_file(f, cx));
                for _ in 0..40 {
                    cx.run_until_parked();
                    std::thread::sleep(Duration::from_millis(25));
                }
                eprintln!(
                    "selected: {:?}",
                    tui.read_with(&cx, |t, _| t.ts.tree_sel.clone())
                );
            }
        }
    }
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

/// What the tabs need to have something to show in a shot: a git repository with a changed file and a trusted
/// folder for the file explorer; two saved browser frames (generated PNGs inside the session's artifact
/// directory) for the browser tab.
fn prepare_tabs(
    live: &mut Live,
    engine: &Engine,
    tmp: &std::path::Path,
    tab: super::ChatTab,
) -> Result<(), Err> {
    let ws = tmp.join("ws");
    match tab {
        super::ChatTab::Files => {
            std::fs::create_dir_all(ws.join("src"))?;
            std::fs::create_dir_all(ws.join("tests"))?;
            std::fs::write(
                ws.join("src/main.rs"),
                "fn main() {\n    println!(\"hello\");\n}\n",
            )?;
            std::fs::write(
                ws.join("src/lib.rs"),
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            )?;
            std::fs::write(
                ws.join("tests/add.rs"),
                "#[test]\nfn adds() {\n    assert_eq!(1 + 1, 2);\n}\n",
            )?;
            std::fs::write(ws.join("README.md"), "# demo\n\nA small project.\n")?;
            let git = |args: &[&str]| {
                std::process::Command::new("git")
                    .args(["-c", "user.email=a@b.c", "-c", "user.name=shot"])
                    .args(args)
                    .current_dir(&ws)
                    .output()
            };
            git(&["init", "-q"])?;
            git(&["add", "-A"])?;
            git(&["commit", "-q", "-m", "init"])?;
            std::fs::write(
                ws.join("src/main.rs"),
                "fn main() {\n    println!(\"hello, world\");\n    let n = 3;\n}\n",
            )?;
            live.trusted = true;
        }
        super::ChatTab::Browser => {
            let dir = engine
                .synthesis_home
                .join("sessions")
                .join(&engine.sid)
                .join("artifacts");
            std::fs::create_dir_all(&dir)?;
            for (i, (url, shade)) in [
                ("http://localhost:3000/login", 40u8),
                ("http://localhost:3000/dashboard", 90u8),
            ]
            .iter()
            .enumerate()
            {
                let path = dir.join(format!("shot-{}.png", i + 1));
                let img = image::RgbaImage::from_fn(640, 400, |x, y| {
                    image::Rgba([
                        shade.saturating_add((x / 4) as u8),
                        60 + (y / 4) as u8,
                        120,
                        255,
                    ])
                });
                img.save(&path)?;
                let ev: synthesis_events::Envelope = serde_json::from_value(serde_json::json!({
                    "v": 1, "seq": 90_000 + i, "ts": "2026-11-07T10:00:0".to_string() + &i.to_string() + "Z",
                    "session_id": engine.sid, "task_id": null, "turn_id": null, "type": "browser.frame",
                    "payload": {"path": path.to_string_lossy(), "url": url, "w": 640, "h": 400,
                                "bytes": std::fs::metadata(&path)?.len()}
                }))?;
                live.view.apply(&ev);
            }
        }
        _ => {}
    }
    Ok(())
}
