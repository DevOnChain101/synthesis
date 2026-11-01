//! Golden cases from `tests/golden/{keys,sgr,selection}` (captured from the Node reference).
//! Every case must pass or be listed with a reason in `golden_divergences.txt`; there are no
//! silent skips. `meta.json.reviewed_by` is not checked here: the cases are run regardless and
//! the count of unreviewed ones is printed.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use synthesis_config::Env;
use synthesis_tui_base::Platform;
use synthesis_tui_base::clipboard::{ClipboardSink, Osc52, PipeClipboard, copy_to_clipboard};
use synthesis_tui_base::keys::{Key, KeyState, Mods, MouseAction, WheelDir, parse_keys};
use synthesis_tui_base::selection::*;
use synthesis_tui_base::sgr::{self, ColorMode, SgrState};

fn golden_dir(area: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden")
        .join(area)
}

fn divergences() -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden_divergences.txt"),
    )
    .unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (case, why) = l.split_once('|').expect("`case | reason`");
            assert!(!why.trim().is_empty(), "divergence without reason: {l}");
            (case.trim().to_string(), why.trim().to_string())
        })
        .collect()
}

fn read(path: PathBuf) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap()
}

/// Runs every case of `area`; panics listing all failures.
fn run_area(area: &str, run: impl Fn(&Value) -> Value) {
    let allowed = divergences();
    let mut dirs: Vec<_> = std::fs::read_dir(golden_dir(area))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "no golden cases found for {area}");
    let (mut passed, mut diverged, mut unreviewed) = (0, 0, 0);
    let mut failures = Vec::new();
    for dir in &dirs {
        let id = format!("{area}/{}", dir.file_name().unwrap().to_string_lossy());
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run(&read(dir.join("input.json")))
        }))
        .ok()
        .filter(|got| *got == read(dir.join("expected.json")));
        if read(dir.join("meta.json"))["reviewed_by"].is_null() {
            unreviewed += 1;
        }
        match (ok.is_some(), allowed.get(&id)) {
            (true, None) => passed += 1,
            (false, Some(_)) => diverged += 1,
            (true, Some(_)) => failures.push(format!(
                "{id}: listed as divergent but passes (stale allow-list entry)"
            )),
            (false, None) => failures.push(format!("{id}: output differs from expected.json")),
        }
    }
    let stale: Vec<_> = allowed
        .keys()
        .filter(|k| {
            k.starts_with(&format!("{area}/"))
                && !dirs
                    .iter()
                    .any(|d| k.ends_with(&*d.file_name().unwrap().to_string_lossy()))
        })
        .collect();
    failures.extend(
        stale
            .iter()
            .map(|k| format!("{k}: allow-list entry has no golden case")),
    );
    println!(
        "GOLDEN {area}: {} cases, {passed} passed, {diverged} divergent (listed), {unreviewed} with reviewed_by unset",
        dirs.len()
    );
    assert!(
        failures.is_empty(),
        "{} failing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------- keys

fn mods_json(m: &Mods) -> [(&'static str, Value); 3] {
    [
        ("shift", m.shift.into()),
        ("alt", m.alt.into()),
        ("ctrl", m.ctrl.into()),
    ]
}

fn key_json(k: &Key) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("name".into(), k.name().into());
    let mut put = |pairs: &[(&str, Value)]| {
        pairs
            .iter()
            .for_each(|(k, v)| drop(o.insert((*k).into(), v.clone())))
    };
    match k {
        Key::Named(_) => {}
        Key::Text(t) | Key::Paste(t) => put(&[("text", t.clone().into())]),
        Key::Mouse {
            x,
            y,
            button,
            action,
            mods,
        } => {
            put(&[
                ("x", (*x).into()),
                ("y", (*y).into()),
                ("button", (*button).into()),
            ]);
            put(&[(
                "action",
                if *action == MouseAction::Down {
                    "down"
                } else {
                    "up"
                }
                .into(),
            )]);
            put(&mods_json(mods));
        }
        Key::MouseDrag { x, y, button, mods } => {
            put(&[
                ("x", (*x).into()),
                ("y", (*y).into()),
                ("button", (*button).into()),
            ]);
            put(&mods_json(mods));
        }
        Key::Wheel { x, y, mods, .. } => {
            put(&[("x", (*x).into()), ("y", (*y).into())]);
            put(&mods_json(mods));
        }
        Key::CursorPosition { row, col } => put(&[("row", (*row).into()), ("col", (*col).into())]),
    }
    Value::Object(o)
}

#[test]
fn golden_keys() {
    run_area("keys", |input| {
        let mut st = KeyState::default();
        let chunks: Vec<Value> = input["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let events: Vec<Value> = parse_keys(c.as_str().unwrap(), &mut st)
                    .iter()
                    .map(key_json)
                    .collect();
                json!({"events": events, "pending": st.pending()})
            })
            .collect();
        json!({ "chunks": chunks })
    });
}

// ---------------------------------------------------------------- sgr

fn colour_json(c: Option<sgr::Colour>) -> Value {
    c.map_or(Value::Null, |c| c.to_string().into())
}

fn mode_of(v: &Value) -> ColorMode {
    match v.as_str() {
        None | Some("truecolor") => ColorMode::TrueColor,
        Some("256") => ColorMode::Ansi256,
        Some(m) => panic!("mode {m}"),
    }
}

#[test]
fn golden_sgr() {
    run_area("sgr", |i| match i["op"].as_str().unwrap() {
        "applySgr" => {
            let mut st = SgrState::default();
            let steps: Vec<Value> = i["steps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    sgr::apply_sgr(&mut st, p.as_str().unwrap());
                    json!({"params": p, "state": {"fg": colour_json(st.fg), "bg": colour_json(st.bg), "int": st.int, "attr": st.attr}})
                })
                .collect();
            Value::Array(steps)
        }
        "detect" => {
            let env = Env::from_pairs(
                i["env"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())),
            );
            let platform = Platform::from_node(i["platform"].as_str().unwrap());
            let mode = match ColorMode::detect(&env, platform) {
                ColorMode::TrueColor => "truecolor",
                ColorMode::Ansi256 => "256",
                m => panic!("unexpected {m:?}"),
            };
            json!({ "mode": mode })
        }
        "encode" => {
            json!({ "result": sgr::encode(i["line"].as_str().unwrap(), mode_of(&i["mode"])) })
        }
        "to256" => Value::Array(
            i["colors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let n = |k: usize| c[k].as_u64().unwrap() as u32;
                    json!({"index": sgr::to256(n(0), n(1), n(2)), "rgb": c})
                })
                .collect(),
        ),
        op => panic!("unknown sgr op {op}"),
    });
}

// ---------------------------------------------------------------- selection

fn pt(v: &Value) -> Point {
    Point {
        x: v["x"].as_i64().unwrap(),
        y: v["y"].as_i64().unwrap(),
    }
}
fn pt_json(p: Point) -> Value {
    json!({"x": p.x, "y": p.y})
}
fn sel(v: &Value) -> Option<Selection> {
    (!v.is_null()).then(|| Selection {
        a: pt(&v["a"]),
        b: pt(&v["b"]),
    })
}
fn sel_json(s: &Selection) -> Value {
    json!({"a": pt_json(s.a), "b": pt_json(s.b)})
}
fn bounds(v: &Value) -> Option<Bounds> {
    let n = |k: &str| v[k].as_i64().unwrap();
    (!v.is_null()).then(|| Bounds {
        x0: n("x0"),
        x1: n("x1"),
        y0: n("y0"),
        y1: n("y1"),
    })
}
fn bounds_json(b: Option<Bounds>) -> Value {
    b.map_or(
        Value::Null,
        |b| json!({"x0": b.x0, "x1": b.x1, "y0": b.y0, "y1": b.y1}),
    )
}
fn geom_json(g: Option<Geom>) -> Value {
    g.map_or(Value::Null, |g| json!({"x0": g.x0, "x1": g.x1, "top": g.top, "h": g.h, "rowAt0": g.row_at0, "total": g.total, "more": g.more}))
}
fn geom_of(v: &Value) -> Option<Geom> {
    let rect = (!v["rect"].is_null()).then(|| Rect {
        x: v["rect"]["x"].as_i64().unwrap(),
        y: v["rect"]["y"].as_i64().unwrap(),
        w: v["rect"]["w"].as_i64().unwrap(),
    });
    let body = (!v["body"].is_null()).then(|| Body {
        total: v["body"]["total"].as_i64().unwrap(),
        h: v["body"]["h"].as_i64().unwrap(),
    });
    body_geom(rect, body, v["scroll"].as_i64().unwrap_or(0)) // JS: `scroll || 0`
}
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn event_of(e: &Value) -> Event {
    let b = |k: &str| e[k].as_bool().unwrap_or(false);
    match e["type"].as_str().unwrap() {
        "down" => Event::Down {
            text: b("text"),
            body: b("body"),
            shift: b("shift"),
            alt: b("alt"),
        },
        "drag" => Event::Drag,
        "up" => Event::Up {
            moved: b("moved"),
            shift: b("shift"),
            alt: b("alt"),
            extending: b("extending"),
        },
        "wheel" => Event::Wheel {
            shift: b("shift"),
            alt: b("alt"),
        },
        "key" => Event::Key { page: b("page") },
        "cancel" => Event::Cancel,
        _ => Event::Unknown,
    }
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Idle => "idle",
        Mode::Dragging => "dragging",
        Mode::Selected => "selected",
        Mode::Pending => "pending",
    }
}

fn mouse_key(e: &Value) -> Key {
    let b = |k: &str| e[k].as_bool().unwrap_or(false);
    let mods = Mods {
        shift: b("shift"),
        alt: b("alt"),
        ctrl: b("ctrl"),
    };
    let (x, y) = (
        e["x"].as_u64().unwrap() as u32,
        e["y"].as_u64().unwrap() as u32,
    );
    match e["name"].as_str().unwrap() {
        "mouse" => Key::Mouse {
            x,
            y,
            button: 0,
            action: if e["action"] == "down" {
                MouseAction::Down
            } else {
                MouseAction::Up
            },
            mods,
        },
        "mouse-drag" => Key::MouseDrag {
            x,
            y,
            button: 0,
            mods,
        },
        "wheel-up" => Key::Wheel {
            dir: WheelDir::Up,
            x,
            y,
            mods,
        },
        n => panic!("mouse event {n}"),
    }
}

/// Records what the fake terminal and fake clip.exe received.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<u8>>>);
impl io::Write for Wire {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// clip.exe stand-in: the real sink's program name and payload, recorded instead of spawned.
type Spawns = Arc<Mutex<Vec<(String, Vec<u8>)>>>;
struct FakeClip(PipeClipboard, Spawns, bool);
impl ClipboardSink for FakeClip {
    fn copy(&self, text: &str) -> io::Result<()> {
        if self.2 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "spawn failed"));
        }
        let program = self
            .0
            .program
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        self.1.lock().unwrap().push((program, self.0.payload(text)));
        Ok(())
    }
}

fn copy_case(i: &Value) -> Value {
    let (osc, spawned) = (Wire::default(), Arc::new(Mutex::new(Vec::new())));
    let fake = FakeClip(
        PipeClipboard::clip_exe(),
        spawned.clone(),
        i["spawnThrows"].as_bool().unwrap_or(false),
    );
    let term = Osc52::new(osc.clone(), i["isTTY"].as_bool().unwrap());
    // the chain `default_chain` builds for the platform, with the tools replaced by fakes
    let chain: Vec<&dyn ClipboardSink> =
        if Platform::from_node(i["platform"].as_str().unwrap()) == Platform::Windows {
            vec![&fake, &term]
        } else {
            vec![&term]
        };
    let returned = copy_to_clipboard(i["text"].as_str().unwrap(), &chain);
    let written = osc.0.lock().unwrap().clone();
    let spawned = spawned.lock().unwrap();
    json!({
        "returned": returned,
        "osc_written": if written.is_empty() { vec![] } else { vec![String::from_utf8(written).unwrap()] },
        // Node records the tail of the absolute path; Rust asserts the real path in clipboard.rs tests
        "spawned_tail": spawned.iter().map(|(p, _)| format!("System32/{p}")).collect::<Vec<_>>(),
        "stdin_base64": spawned.iter().map(|(_, d)| synthesis_tui_base::clipboard::base64(d)).collect::<Vec<_>>(),
    })
}

#[test]
fn golden_selection() {
    run_area("selection", |i| {
        match i["op"].as_str().unwrap() {
        "applySelection" => {
            let lines = strings(&i["lines"]);
            let out = apply_selection(&lines, sel(&i["sel"]).as_ref(), bounds(&i["bounds"]).as_ref());
            json!({"lines": out.as_ref(), "same_reference": matches!(out, std::borrow::Cow::Borrowed(_))})
        }
        "bodyGeom" => Value::Array(
            i["cases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let g = geom_of(c);
                    let mut o = c.as_object().unwrap().clone();
                    o.insert("geom".into(), geom_json(g));
                    o.insert("bounds".into(), bounds_json(g.as_ref().map(body_bounds)));
                    Value::Object(o)
                })
                .collect(),
        ),
        "cellHasText" => Value::Array(i["xs"].as_array().unwrap().iter().map(|x| json!({"x": x, "hasText": cell_has_text(i["line"].as_str().unwrap(), x.as_i64().unwrap())})).collect()),
        "clampPoint" => {
            let b = bounds(&i["bounds"]).unwrap();
            Value::Array(i["points"].as_array().unwrap().iter().map(|p| json!({"p": p, "clamped": pt_json(clamp_point(pt(p), &b))})).collect())
        }
        "copy" => copy_case(i),
        "describeMouse" => Value::Array(i["events"].as_array().unwrap().iter().map(|e| describe_mouse(&mouse_key(e)).unwrap().into()).collect()),
        "edgeScrollStep" => {
            let (y0, y1) = (i["bounds"]["y0"].as_i64().unwrap(), i["bounds"]["y1"].as_i64().unwrap());
            Value::Array(i["ys"].as_array().unwrap().iter().map(|y| json!({"y": y, "step": edge_scroll_step(y.as_i64().unwrap(), y0, y1)})).collect())
        }
        "geomPoints" => {
            let g = geom_of(i).unwrap();
            let content: Vec<Value> = i["points"].as_array().unwrap().iter().map(|p| json!({"screen": p, "content": pt_json(point_to_content(&g, pt(p)))})).collect();
            let to_screen_v: Vec<Value> = i["sels"].as_array().unwrap().iter().map(|s| json!({"sel": s, "screen": sel_json(&to_screen(&sel(s).unwrap(), &g))})).collect();
            json!({"geom": geom_json(Some(g)), "bounds": bounds_json(Some(body_bounds(&g))), "content": content, "toScreen": to_screen_v})
        }
        "highlightCells" => json!({"result": highlight_cells(i["line"].as_str().unwrap(), i["from"].as_i64().unwrap(), i["to"].as_i64().unwrap())}),
        "rowRange" => {
            let (s, b) = (sel(&i["sel"]).unwrap(), bounds(&i["bounds"]).unwrap());
            Value::Array(i["ys"].as_array().unwrap().iter().map(|y| json!({"y": y, "range": row_range(&s, &b, y.as_i64().unwrap()).map_or(Value::Null, |(f, t)| json!([f, t]))})).collect())
        }
        "sliceCells" => Value::Array(i["ranges"].as_array().unwrap().iter().map(|r| json!({"range": r, "text": slice_cells(i["line"].as_str().unwrap(), r[0].as_i64().unwrap(), r[1].as_i64().unwrap())})).collect()),
        "textOfContent" => {
            let from = i["from"].as_i64().unwrap();
            let rows = strings(&i["rows"]);
            let text = text_of_content(&sel(&i["sel"]).unwrap(), i["xb"]["x0"].as_i64().unwrap(), i["xb"]["x1"].as_i64().unwrap(), |a, b| {
                assert_eq!(a, from);
                rows.iter().take((b - a + 1) as usize).cloned().collect()
            });
            json!({ "text": text })
        }
        "textOfSelection" => {
            let lines = strings(&i["lines"]);
            let text = text_of_selection(|y| lines.get(y as usize).cloned().unwrap_or_default(), sel(&i["sel"]).as_ref(), bounds(&i["bounds"]).as_ref());
            json!({ "text": text })
        }
        "transition" => {
            let mode = match i["mode"].as_str().unwrap() {
                "idle" => Mode::Idle,
                "dragging" => Mode::Dragging,
                "selected" => Mode::Selected,
                "pending" => Mode::Pending,
                m => panic!("mode {m}"),
            };
            Value::Array(
                i["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| {
                        let (m, a) = selection_transition(mode, event_of(e));
                        let act = format!("{a:?}").to_lowercase();
                        json!({"event": e, "result": {"mode": mode_name(m), "act": act}})
                    })
                    .collect(),
            )
        }
        op => panic!("unknown selection op {op}"),
    }
    });
}
