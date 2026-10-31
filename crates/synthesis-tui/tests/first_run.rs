//! First run: no model configured. The app opens with the setup card, `/config`, `/themes`, `/help`
//! and quitting work, a typed task is refused inline, and `/reload` asks the host to check again.
//! Also the new screens (Tab, `1`/`2` after Esc, the footer chips) and `/themes` persistence.
#![cfg(unix)]

use std::io;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use synthesis_config::Env;
use synthesis_tui::{ExitReason, ScriptedTty, SetupOptions, TuiSettings, run_setup};

struct Rig {
    tty: ScriptedTty,
    join: Option<JoinHandle<io::Result<ExitReason>>>,
    home: tempfile::TempDir,
}

fn rig(cols: u32, rows: u32, tui: TuiSettings) -> Rig {
    let home = tempfile::tempdir().unwrap();
    let (script, tty) = ScriptedTty::new(cols, rows);
    let dir = home.path().to_path_buf();
    let join = std::thread::spawn(move || {
        let mut o = SetupOptions::new(
            "/work/proj".into(),
            "0.0.0".into(),
            Env::from_pairs([("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]),
            vec![
                "Synthesis needs a model and an API key before it can run a task.".into(),
                "why: no model configured".into(),
                "    export SYNTHESIS_API_KEY=...".into(),
            ],
        );
        o.tui = tui;
        o.synthesis_home = Some(dir);
        run_setup(o, tty)
    });
    Rig {
        tty: script,
        join: Some(join),
        home,
    }
}

impl Rig {
    fn wait_for(&self, needle: &str) {
        let t0 = Instant::now();
        loop {
            let s = self.tty.screen_text();
            if s.contains(needle) {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "timed out waiting for {needle:?}; screen:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn finish(&mut self) -> ExitReason {
        self.join.take().unwrap().join().unwrap().unwrap()
    }
}

#[test]
fn the_app_starts_with_the_setup_card_and_refuses_a_task_inline() {
    let mut r = rig(120, 36, TuiSettings::default());
    r.wait_for("Set up a model to start");
    r.wait_for("export SYNTHESIS_API_KEY=...");
    r.tty.send("fix the bug\r");
    r.wait_for("no model is configured yet, so nothing was sent");
    // /model explains instead of opening an empty picker
    r.tty.send("\x15/model\r");
    r.wait_for("/model: no model is configured yet");
    r.tty.send("\x15/exit\r");
    assert_eq!(r.finish(), ExitReason::Quit);
}

#[test]
fn config_help_and_reload_work_before_a_model_exists() {
    let mut r = rig(120, 36, TuiSettings::default());
    r.wait_for("Set up a model to start");
    r.tty.send("/config\r");
    r.wait_for("LEFT SIDEBAR");
    r.tty.send("\t"); // next group: Appearance
    r.wait_for("Icons");
    r.tty.send("\x1b");
    std::thread::sleep(Duration::from_millis(80)); // a lone Esc, not Alt+/
    r.tty.send("/help\r");
    r.wait_for("dialogs:");
    r.tty.send("/reload\r");
    assert!(matches!(r.finish(), ExitReason::OpenFolder(_)));
}

#[test]
fn themes_picker_saves_the_choice_in_the_user_settings() {
    let mut r = rig(120, 36, TuiSettings::default());
    r.wait_for("Set up a model to start");
    r.tty.send("/themes\r");
    r.wait_for("paints the background");
    r.tty.send("system");
    r.wait_for("your terminal's own background");
    r.tty.send("\r");
    r.wait_for("settings saved");
    let text = std::fs::read_to_string(r.home.path().join("settings.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["synthesis"]["tui"]["theme"], "system");
    let (back, d) = TuiSettings::load_user(r.home.path());
    assert_eq!((back.theme_name(), d.len()), ("system", 0));
    // the frames drawn after the choice paint no background
    let raw = r.tty.raw();
    let after = &raw[raw.rfind("settings saved").unwrap()..];
    assert!(!after.contains("[48;"), "system theme painted a background");
    r.tty.send("/exit\r");
    r.finish();
}

#[test]
fn tab_and_esc_digits_and_the_footer_chips_switch_screens() {
    let mut r = rig(140, 40, TuiSettings::default());
    r.wait_for("1 mission");
    r.wait_for("2 chat");
    // the Mission screen has the Agent status and Context cards
    r.wait_for("AGENT STATUS");
    r.tty.send("\t");
    // the Chat screen replaces the cards by the one-row strip
    let t0 = Instant::now();
    while r.tty.screen_text().contains("AGENT STATUS") {
        assert!(t0.elapsed() < Duration::from_secs(15), "Tab did not switch");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Esc then 1 goes back, a digit typed normally still types
    r.tty.send("\x1b");
    std::thread::sleep(Duration::from_millis(60));
    r.tty.send("1");
    r.wait_for("AGENT STATUS");
    r.tty.send("2");
    std::thread::sleep(Duration::from_millis(60));
    r.tty.send("7");
    r.wait_for("> 7");
    r.tty.send("\x15/exit\r");
    r.finish();
}

#[test]
fn a_click_on_a_footer_chip_picks_the_screen() {
    let mut r = rig(140, 40, TuiSettings::default());
    r.wait_for("AGENT STATUS");
    // find the `2 chat` chip on the bottom-but-one row and click it (SGR mouse, 1-based)
    let screen = r.tty.screen();
    let (row, line) = screen
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.contains("2 chat"))
        .unwrap();
    let col = line.find("2 chat").unwrap() + 2;
    r.tty.send(&format!(
        "\x1b[<0;{col};{}M\x1b[<0;{col};{}m",
        row + 1,
        row + 1
    ));
    let t0 = Instant::now();
    while r.tty.screen_text().contains("AGENT STATUS") {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "the click did not switch"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    r.tty.send("/exit\r");
    r.finish();
}
