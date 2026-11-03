//! First run: no model is configured. A plain card tells what to set up (the API key stays in an
//! environment variable or a file the user manages: nothing here ever asks for or stores a key,
//! ADR-011), `r` checks again, `q` leaves. The same thing the removed new TUI's setup card did,
//! in the classic look.

use std::io::{self, Read, Write};

use synthesis_config::Env;
use synthesis_tui_base::keys::{Key, KeyParser, Named};
use synthesis_tui_base::screen::{BRACKETED_PASTE_ON, Screen};
use synthesis_tui_base::sgr::ColorMode;
use synthesis_tui_base::terminal::{GuardOptions, TerminalGuard};
use synthesis_tui_base::{Platform, text::width};
use synthesis_tui_render::theme::{ACCENT, ACCENT_LIGHT, MUTED, T_BODY, fit, paint};

use crate::tty::{SharedOut, Tty};

pub struct SetupOptions {
    pub version: String,
    pub env: Env,
    /// The card: the first line is the headline, the rest is shown as given (code lines start with
    /// spaces).
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupExit {
    /// `r` / Enter: the host looks at the configuration again.
    Reload,
    Quit,
}

/// The card as screen rows for a `cols` x `rows` terminal.
pub fn card(lines: &[String], version: &str, cols: usize, rows: usize) -> Vec<String> {
    let inner = cols.saturating_sub(6).max(20);
    let mut body: Vec<String> = vec![String::new()];
    for (i, l) in lines.iter().enumerate() {
        let shown: String = l.chars().take(inner).collect();
        body.push(match i {
            0 => paint(ACCENT_LIGHT, &shown),
            _ if l.starts_with("    ") => paint(ACCENT, &shown),
            _ => paint(T_BODY, &shown),
        });
    }
    body.push(String::new());
    body.push(paint(MUTED, "r  check again      q  quit"));
    let mut out = vec![
        format!(
            " {}",
            paint(ACCENT_LIGHT, &format!("✻ SYNTHESIS {version}"))
        ),
        String::new(),
    ];
    out.extend(body.into_iter().map(|l| format!("  {l}")));
    out.truncate(rows);
    out.into_iter().map(|l| fit(&l, cols)).collect()
}

pub fn run_setup(o: SetupOptions, tty: Tty) -> io::Result<SetupExit> {
    let Tty {
        mut input,
        output,
        size,
        raw_mode,
        stop,
        ..
    } = tty;
    let _ = stop;
    let out = SharedOut::new(output);
    let _guard = TerminalGuard::enter(out.clone(), GuardOptions { raw_mode })?;
    let mut screen = Screen::new(out.clone(), ColorMode::detect(&o.env, Platform::current()));
    screen.set_mouse(false);
    screen.enter()?;
    out.clone().write_all(BRACKETED_PASTE_ON.as_bytes())?;
    let mut parser = KeyParser::default();
    let mut buf = [0u8; 256];
    loop {
        let (cols, rows) = size();
        let lines = card(&o.lines, &o.version, cols as usize, rows as usize);
        debug_assert!(lines.iter().all(|l| width(l) <= cols as usize));
        screen.draw(&lines, cols, rows)?;
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(SetupExit::Quit);
        }
        for k in parser.feed(&buf[..n]) {
            match k {
                Key::Text(t) if t == "r" || t == "R" => return Ok(SetupExit::Reload),
                Key::Text(t) if t == "q" || t == "Q" => return Ok(SetupExit::Quit),
                Key::Named(Named::Enter) => return Ok(SetupExit::Reload),
                Key::Named(Named::Escape | Named::CtrlC | Named::CtrlD) => {
                    return Ok(SetupExit::Quit);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tty::ScriptedTty;
    use synthesis_tui_base::text::strip_ansi;

    fn lines() -> Vec<String> {
        vec![
            "Synthesis needs a model and an API key before it can run a task.".into(),
            "Keep the key in an environment variable you manage:".into(),
            "    export MY_KEY=...".into(),
        ]
    }

    #[test]
    fn the_card_fits_the_terminal_and_never_asks_for_a_key() {
        for (c, r) in [(80, 24), (40, 10), (200, 50), (20, 3)] {
            let rows = card(&lines(), "0.1", c, r);
            assert!(rows.len() <= r);
            assert!(rows.iter().all(|l| width(l) <= c), "{c}x{r}");
        }
        let text = strip_ansi(&card(&lines(), "0.1", 100, 24).join("\n"));
        assert!(text.contains("SYNTHESIS 0.1") && text.contains("r  check again"));
        assert!(!text.to_lowercase().contains("paste your key") && !text.contains("enter your"));
    }

    fn run(keys: &'static str) -> SetupExit {
        let (script, tty) = ScriptedTty::new(100, 24);
        let o = SetupOptions {
            version: "0.1".into(),
            env: Env::from_pairs([("TERM", "xterm-256color")]),
            lines: lines(),
        };
        let h = std::thread::spawn(move || run_setup(o, tty).unwrap());
        let t0 = std::time::Instant::now();
        while !script.screen_text().contains("needs a model") {
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "card never drawn:\n{}",
                script.screen_text()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        script.send(keys);
        h.join().unwrap()
    }

    #[test]
    fn r_reloads_and_q_quits() {
        assert_eq!(run("r"), SetupExit::Reload);
        assert_eq!(run("q"), SetupExit::Quit);
        assert_eq!(run("\x03"), SetupExit::Quit);
    }
}
