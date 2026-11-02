//! Runs every case in tests/golden/{tools,schemas} against the Rust tools.
//! Intentional differences are listed (with reasons) in tests/golden-divergences.json; nothing
//! is skipped silently. Run with `--nocapture` for the per-case report.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use serde_json::{Value, json};
use synthesis_core::{PermissionMode, ToolContext, WorkspaceContext};
use synthesis_tools::{Registry, ToolRuntime};

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden")
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn cases(area: &str) -> Vec<(String, PathBuf)> {
    let mut v: Vec<_> = fs::read_dir(golden_dir().join(area))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().join("input.json").exists())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    v.sort();
    v
}

fn divergences() -> Value {
    read_json(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden-divergences.json"))
}

struct Sandbox {
    _dir: tempfile::TempDir,
    tmp: String,
    home: String,
    synthesis_home: String,
    ws: PathBuf,
}

fn set_mtime(p: &Path, secs: u64) {
    // Directories cannot be opened this way on Windows; unverified there.
    // Files need a write handle: on Windows a read-only handle cannot set times (ignored error
    // made every mtime-ordered case come back in name order). Directories fall back to read.
    let t = UNIX_EPOCH + Duration::from_secs(secs);
    let _ = fs::OpenOptions::new()
        .write(true)
        .open(p)
        .or_else(|_| fs::File::open(p))
        .and_then(|f| f.set_modified(t));
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // No `\\?\` prefix, and "/" spelling on Windows so the POSIX-spelled fixtures compose.
        let tmp = synthesis_core::canonicalize(dir.path()).unwrap();
        let tmp = if cfg!(windows) {
            PathBuf::from(tmp.to_string_lossy().replace('\\', "/"))
        } else {
            tmp
        };
        for d in ["home", "synthesis_home", "ws", "outside"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        Self {
            home: s(tmp.join("home")),
            synthesis_home: s(tmp.join("synthesis_home")),
            ws: tmp.join("ws"),
            tmp: s(tmp),
            _dir: dir,
        }
    }
    fn expand_str(&self, s: &str) -> String {
        s.replace("<SYNT_HOME>", &self.synthesis_home)
            .replace("<HOME>", &self.home)
            .replace("<TMP>", &self.tmp)
    }
    /// Longest root first, as the README prescribes.
    fn normalize_str(&self, s: &str) -> String {
        self.normalize_with(s, cfg!(windows))
    }
    /// With `win`, tools may re-spell our paths with backslashes: a root in either spelling is
    /// replaced and `\` -> `/` is folded only in the path token right after it, never in content.
    fn normalize_with(&self, s: &str, win: bool) -> String {
        let roots = [
            (&self.synthesis_home, "<SYNT_HOME>"),
            (&self.home, "<HOME>"),
            (&self.tmp, "<TMP>"),
        ];
        let same = |a: u8, b: u8| a == b || (win && a == b'\\' && b == b'/');
        let mut out = String::new();
        let mut i = 0;
        'scan: while i < s.len() {
            for (root, ph) in roots {
                let n = root.len();
                if s.len() - i >= n
                    && s.is_char_boundary(i + n)
                    && s.as_bytes()[i..i + n]
                        .iter()
                        .zip(root.bytes())
                        .all(|(&a, b)| same(a, b))
                {
                    out.push_str(ph);
                    i += n;
                    while let Some(c) = s[i..].chars().next() {
                        if c.is_whitespace() || c == '"' || c == '\'' {
                            break;
                        }
                        out.push(if win && c == '\\' { '/' } else { c });
                        i += c.len_utf8();
                    }
                    continue 'scan;
                }
            }
            let c = s[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
        out
    }
    fn map(&self, v: &Value, f: &dyn Fn(&Self, &str) -> String) -> Value {
        match v {
            Value::String(s) => Value::String(f(self, s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.map(x, f)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (f(self, k), self.map(x, f)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    /// Files get mtime 1700000000+i in order (oldest first); directories are pinned newer than
    /// every file, newest = the one holding the latest file (stable on coarse-clock filesystems).
    fn setup(&self, input: &Value, order: Option<&Vec<String>>) {
        for d in input["dirs"].as_array().unwrap() {
            fs::create_dir_all(d.as_str().unwrap()).unwrap();
        }
        let mut files: Vec<(&String, &Value)> =
            input["files"].as_object().unwrap().iter().collect();
        if let Some(order) = order {
            let pos = |k: &String| {
                let rel = k.strip_prefix(&format!("{}/ws/", self.tmp)).unwrap_or(k);
                order.iter().position(|o| o == rel).unwrap_or(usize::MAX)
            };
            files.sort_by_key(|(k, _)| pos(k));
        }
        let mut dir_rank: HashMap<PathBuf, u64> = HashMap::new();
        for (i, (k, c)) in files.into_iter().enumerate() {
            let p = PathBuf::from(k);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            let bytes = match c {
                Value::String(s) => s.clone().into_bytes(),
                Value::Object(o) if o.contains_key("base64") => {
                    base64::engine::general_purpose::STANDARD
                        .decode(o["base64"].as_str().unwrap())
                        .unwrap()
                }
                other => serde_json::to_string_pretty(other).unwrap().into_bytes(),
            };
            fs::write(&p, bytes).unwrap();
            set_mtime(&p, 1_700_000_000 + i as u64);
            for a in p.ancestors().skip(1).take_while(|a| *a != self.ws) {
                dir_rank.insert(a.to_path_buf(), 1_800_000_000 + i as u64);
            }
        }
        for (d, t) in dir_rank {
            set_mtime(&d, t);
        }
    }
    fn tree(&self, root: &Path) -> Vec<Value> {
        let mut out = vec![];
        let mut names: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            let rel = p
                .strip_prefix(&self.ws)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                out.push(json!({"path": rel, "type": "dir"}));
                out.extend(self.tree(&p));
            } else {
                let b = fs::read(&p).unwrap();
                if b.contains(&0) {
                    let d = base64::engine::general_purpose::STANDARD.encode(&b);
                    out.push(json!({"path": rel, "type": "file", "base64": d}));
                } else {
                    let s = String::from_utf8_lossy(&b).into_owned();
                    out.push(json!({"path": rel, "type": "file", "content": s}));
                }
            }
        }
        out
    }
}

fn tool_ctx(sb: &Sandbox) -> ToolContext {
    let ws =
        WorkspaceContext::open("w", &sb.ws, &sb.ws, &[], sb.ws.join("../artifacts"), "pg").unwrap();
    ToolContext {
        session_id: "s".into(),
        task_id: "t".into(),
        turn_id: "u".into(),
        tool_call_id: "c".into(),
        workspace: Arc::new(ws),
        env: Arc::new(BTreeMap::new()),
        read_ledger: Arc::new(Mutex::new(HashMap::new())),
        permission_mode: PermissionMode::Default,
        skip_secrets: false,
        sub_agent: false,
        approval_token: None,
        deadline: None,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

fn sorted_lines(v: &Value) -> Value {
    match v.as_str() {
        Some(s) => {
            let mut l: Vec<&str> = s.split('\n').collect();
            l.sort();
            Value::String(l.join("\n"))
        }
        None => v.clone(),
    }
}

/// Returns mismatch descriptions (empty = pass). Calls `Tool::execute` directly, so the tools'
/// own required-field checks are exercised; schema validation is covered by `registry_*` tests.
fn run_tools_case(
    reg: &Registry,
    rt: &tokio::runtime::Runtime,
    id: &str,
    dir: &Path,
    div: &Value,
) -> Vec<String> {
    let sb = Sandbox::new();
    let input = sb.map(&read_json(&dir.join("input.json")), &Sandbox::expand_str);
    let expected = read_json(&dir.join("expected.json"));
    let order = div["mtime_order"][id].as_array().map(|a| {
        a.iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    });
    sb.setup(&input, order.as_ref());
    let ctx = tool_ctx(&sb);
    let over = &div["cases"][id]["steps"];
    let mut bad = vec![];
    let exp_steps = expected["steps"].as_array().unwrap();
    for (n, step) in input["steps"].as_array().unwrap().iter().enumerate() {
        let tool = step["tool"].as_str().unwrap();
        let exp = &exp_steps[n];
        if tool == "__write" {
            let i = &step["input"];
            let p = sb.ws.join(i["path"].as_str().unwrap());
            fs::write(&p, i["content"].as_str().unwrap()).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(i["mtime"].as_u64().unwrap()))
                .unwrap();
            continue;
        }
        let t = reg
            .get(tool)
            .unwrap_or_else(|| panic!("{id}: no tool {tool}"));
        let out = rt.block_on(t.execute(step.get("input").cloned().unwrap_or(Value::Null), &ctx));
        let mut content = sb.map(
            &serde_json::to_value(&out.content).unwrap(),
            &Sandbox::normalize_str,
        );
        let mut want = exp["content"].clone();
        if step.get("sort_lines").is_some() {
            content = sorted_lines(&content);
            want = sorted_lines(&want);
        }
        let o = &over[n.to_string()];
        let ok = if let Some(p) = o["content_prefix"].as_str() {
            content.as_str().is_some_and(|c| c.starts_with(p))
        } else if let Some(c) = o.get("content") {
            &content == c
        } else {
            content == want
        };
        if !ok {
            bad.push(format!(
                "step {n} ({tool}) content:\n  got:  {content}\n  want: {want}"
            ));
        }
        if exp["isError"] != json!(out.is_error) || exp["soft"] != json!(out.soft) {
            bad.push(format!(
                "step {n} ({tool}) flags: got isError={} soft={}, want {} {}",
                out.is_error, out.soft, exp["isError"], exp["soft"]
            ));
        }
        // `meta.diff` is an additive field of the Rust port (the UI's line-numbered diff, see
        // docs/specs/tools.md); the captured Node meta never had it. It is tested in tests/diff_meta.rs.
        let mut meta = (!out.meta.is_null()).then(|| out.meta.clone());
        if let Some(m) = meta.as_mut().and_then(Value::as_object_mut) {
            m.remove("diff");
        }
        if exp.get("meta").cloned() != meta {
            bad.push(format!(
                "step {n} ({tool}) meta: got {meta:?}, want {:?}",
                exp.get("meta")
            ));
        }
    }
    // Files given as JSON objects are compared as parsed JSON: input.json sorted their keys,
    // so the byte layout differs from the capture.
    let tree = sb.tree(&sb.ws);
    let want_tree = expected["tree_after"].as_array().unwrap();
    let same = |a: &Value, b: &Value| {
        a == b || {
            let parse = |v: &Value| {
                v["content"]
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
            };
            a["path"] == b["path"] && parse(a).is_some() && parse(a) == parse(b)
        }
    };
    if tree.len() != want_tree.len() || !tree.iter().zip(want_tree).all(|(a, b)| same(a, b)) {
        bad.push(format!(
            "tree_after:\n  got:  {}\n  want: {}",
            Value::Array(tree),
            Value::Array(want_tree.clone())
        ));
    }
    bad
}

fn run_schema_case(reg: &Registry, dir: &Path) -> Vec<String> {
    let input = read_json(&dir.join("input.json"));
    let expected = read_json(&dir.join("expected.json"));
    match input["op"].as_str().unwrap() {
        "names" => {
            let by = |rt: ToolRuntime| -> Vec<String> {
                reg.specs()
                    .filter(|s| s.runtime == rt)
                    .map(|s| s.name.clone())
                    .collect()
            };
            let got = json!({
                "builtin": by(ToolRuntime::Builtin),
                "agent": by(ToolRuntime::Harness),
                "schema_keys": ["description", "input_schema", "name"],
            });
            if got == expected {
                vec![]
            } else {
                vec![format!("got {got}\nwant {expected}")]
            }
        }
        _ => {
            let name = input["name"].as_str().unwrap();
            let extra = divergences()["schema_extra_properties"][name].clone();
            let got = reg.specs().find(|s| s.name == name).map(|s| {
                let mut schema = s.input_schema.clone();
                // properties added on purpose (see golden-divergences.json); the rest must match
                for p in extra
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    schema["properties"].as_object_mut().map(|m| m.remove(p));
                }
                // enum values added on purpose
                for (p, vals) in divergences()["schema_enum_extra"][name]
                    .as_object()
                    .into_iter()
                    .flatten()
                {
                    if let Some(e) = schema["properties"][p]["enum"].as_array_mut() {
                        e.retain(|v| !vals.as_array().is_some_and(|a| a.contains(v)));
                    }
                }
                json!({"name": s.name, "description": s.description, "input_schema": schema})
            });
            if got.as_ref() == Some(&expected) {
                vec![]
            } else {
                vec![format!("schema for {name} differs")]
            }
        }
    }
}

/// A string in the case names a POSIX absolute path (`/etc/x`), which cannot exist on Windows.
fn posix_only(v: &Value) -> bool {
    let bad = |s: &str| {
        let b = s.as_bytes();
        (0..b.len()).any(|i| {
            b[i] == b'/'
                && (i == 0 || b" \t(=\"'`,:".contains(&b[i - 1]))
                && b.get(i + 1)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || b"_.~*".contains(c))
        })
    };
    match v {
        Value::String(s) => bad(s),
        Value::Array(a) => a.iter().any(posix_only),
        Value::Object(o) => o.iter().any(|(k, x)| bad(k) || posix_only(x)),
        _ => false,
    }
}

#[test]
fn golden_cases() {
    let reg = Registry::unsandboxed_for_tests();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let div = divergences();
    let mut failures = vec![];
    let mut skipped = 0;
    for area in ["schemas", "tools"] {
        let (mut pass, mut total, mut diverged) = (0, 0, 0);
        for (id, dir) in cases(area) {
            total += 1;
            if cfg!(windows) && posix_only(&read_json(&dir.join("input.json"))) {
                skipped += 1;
                println!(
                    "SKIP {area}/{id}: input names POSIX absolute paths, impossible on Windows"
                );
                continue;
            }
            let bad = if area == "schemas" {
                run_schema_case(&reg, &dir)
            } else {
                run_tools_case(&reg, &rt, &id, &dir, &div)
            };
            let is_div = !div["cases"][&id].is_null();
            diverged += usize::from(is_div);
            println!(
                "{} {area}/{id}{}",
                if bad.is_empty() { "PASS" } else { "FAIL" },
                if is_div {
                    "  [documented divergence]"
                } else {
                    ""
                }
            );
            if bad.is_empty() {
                pass += 1;
            } else {
                let msg = format!("{area}/{id}:\n{}", bad.join("\n"));
                eprintln!("FAIL {msg}"); // case id, got and want, even if the summary is cut
                failures.push(msg);
            }
        }
        println!(
            "== {area}: {pass}/{total} pass ({diverged} with documented divergence overrides)"
        );
    }
    // Documented: read-image-returns-base64-block. More means the skip rule hides cases.
    assert!(skipped <= 1, "{skipped} golden cases skipped");
    assert!(
        failures.is_empty(),
        "{} golden case(s) failed:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn every_divergence_names_an_existing_case() {
    let div = divergences();
    for key in ["cases", "mtime_order"] {
        for id in div[key].as_object().unwrap().keys() {
            assert!(
                golden_dir()
                    .join("tools")
                    .join(id)
                    .join("input.json")
                    .exists(),
                "{key}: unknown case {id}"
            );
        }
    }
}

#[test]
fn windows_normaliser_folds_only_path_tokens_not_content() {
    let sb = Sandbox::new();
    let t = sb.tmp.clone();
    let bs = t.replace('/', "\\");
    let input = format!("Edited {bs}\\ws\\a.txt:\n 1\tprice \\n $& {bs}\\home\\x");
    assert_eq!(
        sb.normalize_with(&input, true),
        "Edited <TMP>/ws/a.txt:\n 1\tprice \\n $& <HOME>/x"
    );
}
