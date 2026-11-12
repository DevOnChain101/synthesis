//! Desktop parity with the CLI's input rules: dropped images follow the `@file` rule,
//! `!cmd` and `#note` work as in the CLI, Shift+Tab never reaches bypassPermissions. The real engine
//! on a scripted model, a temp `SYNT_HOME`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use synthesis_config::Env;
use synthesis_desktop::engine::for_tests;
use synthesis_desktop::engine::{Engine, UiMsg};
use synthesis_desktop::live::{DialogKind, Live, PickerKind, Submitted};
use synthesis_permissions::Mode;
use synthesis_view::convo::{Item, ToolState};

struct Rig {
    live: Live,
    rx: UnboundedReceiver<UiMsg>,
    engine: Arc<Engine>,
    ws: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let sp = root.join("script.json");
    std::fs::write(&sp, r#"[{"text": "It printed hello."}]"#).unwrap();
    let env = Env::from_pairs([
        ("HOME", home.to_str().unwrap()),
        ("SYNT_HOME", home.to_str().unwrap()),
    ]);
    let (engine, rx) = for_tests::start(
        &ws,
        &env,
        None,
        Some(&format!("fake:{}", sp.display())),
        Some("off"),
    )
    .unwrap();
    let engine = Arc::new(engine);
    let mut live = Live::new();
    live.attach(engine.clone(), None);
    Rig {
        live,
        rx,
        engine,
        ws,
        _dir: dir,
    }
}

impl Rig {
    fn pump(&mut self, what: &str, stop: impl Fn(&Live) -> bool) {
        let t0 = Instant::now();
        while !stop(&self.live) {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {what}");
            match self.rx.try_recv() {
                Ok(m) => {
                    self.live.on_msg(m, 1_000_000);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

fn png() -> Vec<u8> {
    b"\x89PNG\r\n\x1a\n".to_vec()
}

#[test]
fn dropped_images_follow_the_workspace_and_secret_rules() {
    let r = rig();
    let outside = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(outside.path()).unwrap();
    let ok = r.ws.join("shot.png");
    std::fs::write(&ok, png()).unwrap();
    // the engine's realpath has no `\\?\` prefix on Windows, which `std::fs::canonicalize` adds
    assert_eq!(
        r.engine.image_path(&ok).unwrap(),
        synthesis_core::canonicalize(&ok).unwrap()
    );

    let far = outside.join("far.png");
    std::fs::write(&far, png()).unwrap();
    assert!(r.engine.image_path(&far).is_err(), "outside the workspace");
    assert!(r.engine.image_path(&r.ws.join("missing.png")).is_err());

    // a secret name, and a link inside the workspace to a secret file
    let env_png = r.ws.join(".env.png");
    std::fs::write(&env_png, png()).unwrap();
    assert!(r.engine.image_path(&env_png).is_err(), "secret name");
    let secret = r.ws.join(".env");
    std::fs::write(&secret, "K=v").unwrap();
    #[cfg(unix)]
    {
        let link = r.ws.join("innocent.png");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        assert!(r.engine.image_path(&link).is_err(), "link to a secret");
        // a link that leaves the workspace resolves outside
        let out_link = r.ws.join("out.png");
        std::os::unix::fs::symlink(&far, &out_link).unwrap();
        assert!(r.engine.image_path(&out_link).is_err(), "link out");
    }
}

#[test]
fn bang_runs_through_the_harness_and_plan_mode_refuses() {
    let mut r = rig();
    assert_eq!(
        r.live.submit("!echo hello-from-user"),
        Submitted::Send("!echo hello-from-user".into())
    );
    r.engine
        .user_command("echo hello-from-user".into(), false)
        .unwrap();
    r.pump("done", |l| {
        !l.busy
            && l.view.convo.items.iter().any(|i| {
                matches!(
                    i,
                    Item::Tool {
                        state: ToolState::Ok,
                        ..
                    }
                )
            })
    });
    let out = r
        .live
        .view
        .convo
        .items
        .iter()
        .find_map(|i| match i {
            Item::Tool { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(out.contains("hello-from-user"), "{out}");
    assert!(r.engine.take_followup(), "the model reads the output");
    assert!(!r.engine.take_followup(), "once");
    // `!!` only records it
    r.engine.user_command("true".into(), true).unwrap();
    r.pump("done 2", |l| {
        l.view
            .convo
            .items
            .iter()
            .filter(|i| matches!(i, Item::Tool { .. }))
            .count()
            == 2
    });
    // wait for the session to come back
    let t0 = Instant::now();
    while r.engine.running() {
        assert!(t0.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!r.engine.take_followup());
    r.engine.set_mode(Mode::Plan);
    let e = r.engine.user_command("echo no".into(), false).unwrap_err();
    assert!(e.contains("plan mode"), "{e}");
}

#[test]
fn hash_asks_which_file_and_saves_only_after_the_pick() {
    let mut r = rig();
    assert_eq!(r.live.submit("#  prefer tabs  "), Submitted::Nothing);
    let d = &r.live.dialogs[0];
    assert!(matches!(
        d.kind,
        DialogKind::Picker {
            kind: PickerKind::Remember,
            ..
        }
    ));
    assert_eq!(d.options[0].0, "no", "the default saves nothing");
    let file = r.engine.notes.project.clone();
    assert!(!file.exists());
    // Esc saves nothing
    r.live.reject();
    assert!(!file.exists());
    r.live.submit("# prefer tabs");
    let t = r.live.dialogs[0].armed_until;
    assert!(t > 0, "armed against type-ahead like the approval dialogs");
    assert_eq!(r.live.pick(1, t - 1), None, "refused while arming");
    let k = r.live.pick(1, t).unwrap();
    assert_eq!(k, "project");
    r.live.answer(&k, t);
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("- prefer tabs")
    );
    assert!(r.live.dialogs.is_empty());
}

#[test]
fn shift_tab_reaches_bypass_only_through_its_confirmation() {
    let mut r = rig();
    for _ in 0..3 {
        r.live.cycle_mode();
    }
    assert_eq!(r.engine.mode(), Mode::Auto);
    // the fourth press asks; the mode does not move
    r.live.cycle_mode();
    assert_eq!(r.engine.mode(), Mode::Auto);
    assert_eq!(r.live.dialogs.len(), 1);
    // a second press while it is open does not stack another
    r.live.cycle_mode();
    assert_eq!(r.live.dialogs.len(), 1);
    // Esc cancels: still auto
    r.live.reject();
    assert!(r.live.dialogs.is_empty());
    assert_eq!(r.engine.mode(), Mode::Auto);
    // Enter on the default (first) option is Cancel
    r.live.cycle_mode();
    assert_eq!(r.live.dialogs[0].sel, 0);
    assert_eq!(r.live.dialogs[0].options[0].0, "cancel");
    r.live.answer("cancel", 0);
    assert_eq!(r.engine.mode(), Mode::Auto);
    // the explicit option enables it, read back from the engine
    r.live.cycle_mode();
    r.live.answer("enable", 0);
    assert_eq!(r.engine.mode(), Mode::BypassPermissions);
    // leaving it needs no dialog
    r.live.cycle_mode();
    assert_eq!(r.engine.mode(), Mode::Default);
    assert!(r.live.dialogs.is_empty());
}

#[test]
fn hash_without_an_engine_says_so_and_esc_forgets_the_note() {
    let mut l = Live::new();
    assert_eq!(l.submit("# hello"), Submitted::Nothing);
    assert!(l.dialogs.is_empty());
    assert!(matches!(
        l.view.convo.items.last(),
        Some(Item::Notice { warning: true, .. })
    ));
    let mut r = rig();
    r.live.submit("# a note");
    r.live.reject();
    assert!(r.live.dialogs.is_empty());
    // a later pick of a stale dialog cannot save the old note: nothing is pending
    r.live.submit("# other");
    r.live.reject();
    assert!(!r.engine.notes.project.exists());
}

#[cfg(unix)]
#[test]
fn a_link_swapped_in_after_the_check_is_not_read() {
    use synthesis_desktop::tui_view::driver::read_image_file;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("a.png");
    std::fs::write(&real, png()).unwrap();
    assert_eq!(read_image_file(&real).unwrap(), (true, png()));
    let link = dir.path().join("b.png");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(read_image_file(&link).is_err(), "O_NOFOLLOW refuses a link");
    let big = dir.path().join("big.png");
    std::fs::write(&big, vec![0u8; (4 << 20) + 10]).unwrap();
    assert!(read_image_file(&big).is_err(), "over the cap");
    assert!(read_image_file(dir.path()).is_err(), "a directory");
}
