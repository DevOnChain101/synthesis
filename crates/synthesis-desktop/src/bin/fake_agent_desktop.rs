// Test double for `agent-desktop` (docs/DESKTOP-CONTROL.md). Replays canned envelopes from
// `$AGENT_DESKTOP_HOME/fake.json`: `{"<command>": <envelope> | [<envelope>, ...]}` (a list is
// consumed one entry per call, the last one repeats). Every call's arguments are appended to
// `$AGENT_DESKTOP_HOME/calls.log`, one JSON array per line; the names of its environment variables go to `env.log`. An entry may carry
// `"_fake": {"sleep_ms": N, "png": true, "flood": N}`; it is removed before printing.
// Not agent-desktop, never shipped (releases build `-p synthesis-cli` only), no OS API is touched.
// It reads AGENT_DESKTOP_HOME from the process env on purpose: that is the contract it fakes.

use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // "App under test" modes, so tests need no shell: `__app <pidfile>` logs to both streams,
    // starts a child that sleeps, records the child's pid and sleeps; `__sleep` just sleeps.
    match args.first().map(String::as_str) {
        Some("__sleep") => return std::thread::sleep(std::time::Duration::from_secs(61)),
        Some("__app") => {
            println!("hello from app");
            eprintln!("boom on stderr");
            if let Ok(child) = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("__sleep")
                .spawn()
                && let Some(f) = args.get(1)
            {
                let _ = std::fs::write(f, child.id().to_string());
            }
            return std::thread::sleep(std::time::Duration::from_secs(62));
        }
        _ => {}
    }
    let Some(home) = std::env::var_os("AGENT_DESKTOP_HOME").map(PathBuf::from) else {
        eprintln!("fake-agent-desktop: AGENT_DESKTOP_HOME is not set");
        std::process::exit(2);
    };
    let log = home.join("calls.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        let _ = writeln!(f, "{}", serde_json::to_string(&args).unwrap());
    }
    let names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let _ = std::fs::write(home.join("env.log"), names.join("\n"));
    let script = home.join("fake.json");
    let mut all: Value = std::fs::read_to_string(&script)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let cmd = args.first().cloned().unwrap_or_default();
    let mut entry = match all.get_mut(&cmd) {
        Some(Value::Array(a)) if a.len() > 1 => a.remove(0),
        Some(Value::Array(a)) if a.len() == 1 => a[0].clone(),
        Some(v) if !v.is_array() => v.clone(),
        _ => {
            serde_json::json!({"version":"2.5","ok":false,"command":cmd,"error":{"code":"INVALID_ARGS","message":"fake: no canned answer","disposition":{"delivery":"not_delivered","retry":"safe"}}})
        }
    };
    let fake = entry.as_object_mut().and_then(|o| o.remove("_fake"));
    if let Some(f) = &fake {
        if let Some(ms) = f["sleep_ms"].as_u64() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        if f["png"] == true
            && let Some(path) = args.last()
        {
            let _ = std::fs::write(path, b"\x89PNG\r\n\x1a\nfake");
        }
        if let Some(n) = f["flood"].as_u64() {
            let chunk = "x".repeat(65536);
            let mut out = std::io::stdout();
            for _ in 0..(n / 65536 + 1) {
                if out.write_all(chunk.as_bytes()).is_err() {
                    break;
                }
            }
            return;
        }
    }
    let _ = std::fs::write(&script, serde_json::to_string(&all).unwrap());
    println!("{entry}");
}
