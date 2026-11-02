//! Replays tests/golden/permissions/* (captured from Node `permissions.js`, see tests/golden/README.md).
//! Every case is run, reviewed or not: `meta.json.reviewed_by` only decides whether a case counts
//! as spec, and a mismatch here is worth knowing about either way.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_permissions::{
    AutoModeSettings, Decision, Env, Mode, PermissionRules, Permissions, RuleSpec, Suggestion,
    Verdict, is_auto_safe_bash, is_broad_allow, is_read_only_bash, is_safe_bash_command,
};

fn map_strings(v: &Value, f: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::String(s) => Value::String(f(s)),
        Value::Array(a) => Value::Array(a.iter().map(|x| map_strings(x, f)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (f(k), map_strings(x, f))).collect())
        }
        other => other.clone(),
    }
}

struct Sandbox {
    _dir: tempfile::TempDir,
    tmp: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // dunce: no `\\?\` verbatim prefix; "/" spelling on Windows so the POSIX-spelled fixtures compose
        let tmp = dunce::canonicalize(dir.path()).unwrap();
        let tmp = if cfg!(windows) {
            PathBuf::from(tmp.to_string_lossy().replace('\\', "/"))
        } else {
            tmp
        };
        for d in ["home", "synthesis_home", "ws", "outside"] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        Self { _dir: dir, tmp }
    }
    fn expand(&self, v: &Value) -> Value {
        let t = self.tmp.to_string_lossy().into_owned();
        map_strings(v, &|s| {
            s.replace("<SYNT_HOME>", &format!("{t}/synthesis_home"))
                .replace("<HOME>", &format!("{t}/home"))
                .replace("<TMP>", &t)
        })
    }
    /// Placeholder form of a string, longest root first; a root only matches before a non-name char.
    fn normalize(&self, s: &str) -> String {
        let t = self.tmp.to_string_lossy().into_owned();
        let mut s = s.to_string();
        if cfg!(windows) && s.replace('\\', "/").contains(&t) {
            s = s.replace('\\', "/"); // the engine may re-spell our paths with backslashes
        }
        for (root, ph) in [
            (format!("{t}/synthesis_home"), "<SYNT_HOME>"),
            (format!("{t}/home"), "<HOME>"),
            (t, "<TMP>"),
        ] {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(i) = rest.find(&root) {
                let after = &rest[i + root.len()..];
                let boundary = !after
                    .starts_with(|c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
                out.push_str(&rest[..i]);
                out.push_str(if boundary { ph } else { &root });
                rest = after;
            }
            out.push_str(rest);
            s = out;
        }
        s
    }
    fn setup(&self, fs_spec: &Value) -> bool {
        for d in fs_spec["dirs"].as_array().into_iter().flatten() {
            fs::create_dir_all(d.as_str().unwrap()).unwrap();
        }
        for (p, c) in fs_spec["files"].as_object().into_iter().flatten() {
            fs::create_dir_all(Path::new(p).parent().unwrap()).unwrap();
            fs::write(p, c.as_str().unwrap_or("")).unwrap();
        }
        let links = fs_spec["symlinks"].as_object().filter(|m| !m.is_empty());
        #[cfg(not(unix))]
        if links.is_some() {
            return false; // symlink cases need privileges on Windows: skipped there
        }
        #[cfg(unix)]
        for (link, target) in links.into_iter().flatten() {
            std::os::unix::fs::symlink(target.as_str().unwrap(), link)
                .unwrap_or_else(|e| panic!("symlink {link} -> {target}: {e}"));
        }
        true
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn permissions(i: &Value, mode: Mode) -> Permissions {
    let r = &i["rules"];
    let rules = PermissionRules {
        allow: strings(&r["allow"]),
        deny: strings(&r["deny"]),
        ask: strings(&r["ask"]),
        additional_directories: strings(&r["additionalDirectories"])
            .into_iter()
            .map(PathBuf::from)
            .collect(),
    };
    let cwd = i["cwd"].as_str().unwrap_or("");
    let home = i["home"].as_str().unwrap_or("");
    let mut env = Env::new(cwd, home, mode).interactive(true);
    env.auto_mode = AutoModeSettings {
        classify_all_shell: i["autoMode"]["classifyAllShell"] == json!(true),
        use_auto_mode_during_plan: i["autoMode"]["useAutoModeDuringPlan"] != json!(false),
    };
    let mut p = Permissions::new(rules, env);
    p.block_outside_reads = i["flags"]["blockOutsideReads"] == json!(true);
    p.outside_reads_answered = i["flags"]["outsideReadsAnswered"] == json!(true);
    p
}

fn decision(d: Decision) -> &'static str {
    match d {
        Decision::Allow => "allow",
        Decision::Ask => "ask",
        Decision::Deny => "deny",
        Decision::Classify => "classify",
    }
}

fn verdict_json(sb: &Sandbox, v: &Verdict) -> Value {
    let mut o = json!({ "decision": decision(v.decision), "reason": v.reason });
    if v.critical {
        o["critical"] = json!(true);
    }
    if v.rule {
        o["rule"] = json!(true);
    }
    if let Some(p) = &v.outside_read {
        o["outsideRead"] = json!(sb.normalize(&p.to_string_lossy()));
    }
    if let Some(m) = v.message {
        o["message"] = json!(m);
    }
    o
}

fn suggestions_json(s: &[Suggestion]) -> Value {
    s.iter()
        .map(|s| match s {
            Suggestion::SetMode(m) => json!({ "type": "setMode", "mode": m.as_str(), "destination": "session" }),
            Suggestion::AddRules { rules, behavior } => json!({
                "type": "addRules",
                "behavior": decision(*behavior),
                "destination": "session",
                "rules": rules.iter().map(|r| json!({ "toolName": r.tool_name, "ruleContent": r.rule_content })).collect::<Vec<_>>(),
            }),
        })
        .collect()
}

fn parse_suggestions(v: &Value) -> Vec<Suggestion> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            if s["type"] == "setMode" {
                Suggestion::SetMode(Mode::parse(s["mode"].as_str().unwrap()).unwrap())
            } else {
                Suggestion::AddRules {
                    behavior: match s["behavior"].as_str() {
                        Some("deny") => Decision::Deny,
                        Some("ask") => Decision::Ask,
                        _ => Decision::Allow,
                    },
                    rules: s["rules"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| RuleSpec {
                            tool_name: r["toolName"].as_str().unwrap().to_string(),
                            rule_content: r["ruleContent"].as_str().map(str::to_string),
                        })
                        .collect(),
                }
            }
        })
        .collect()
}

/// True when a string in the case names a POSIX absolute path outside the `<TMP>` roots
/// (`/etc/x`, `Read(/usr/**)`, `rm -rf /usr`): those cannot exist on Windows.
fn posix_only(v: &Value) -> bool {
    fn bad(s: &str) -> bool {
        let b = s.as_bytes();
        (0..b.len()).any(|i| {
            b[i] == b'/'
                && (i == 0 || b" \t(=\"'`,:".contains(&b[i - 1]))
                && b.get(i + 1)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || b"_.~*".contains(c))
        })
    }
    match v {
        Value::String(s) => bad(s),
        Value::Array(a) => a.iter().any(posix_only),
        Value::Object(o) => o.iter().any(|(k, x)| bad(k) || posix_only(x)),
        _ => false,
    }
}

/// `None` = case skipped on this platform.
fn run_case(i: &Value) -> Option<Value> {
    let sb = Sandbox::new();
    run_case_in(&sb, i).map(|v| map_strings(&v, &|s| sb.normalize(s)))
}

fn run_case_in(sb: &Sandbox, i: &Value) -> Option<Value> {
    let op = i["op"].as_str().unwrap();
    if op == "predicates" {
        return Some(
            i["commands"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let c = c.as_str().unwrap();
                    json!({
                        "command": c,
                        "readOnly": is_read_only_bash(c),
                        "autoSafe": is_auto_safe_bash(c),
                        "isSafeBashCommandIsAutoSafe": is_safe_bash_command(c) == is_auto_safe_bash(c),
                    })
                })
                .collect(),
        );
    }
    if op == "broad" {
        let all = i["classifyAllShell"] == json!(true);
        return Some(
            i["rules"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| json!({ "rule": r, "broad": is_broad_allow(r.as_str().unwrap(), all) }))
                .collect(),
        );
    }
    if cfg!(windows) && posix_only(i) {
        eprintln!("SKIP (windows): input names POSIX absolute paths (/etc, /usr, ...)");
        return None;
    }
    let mut i = sb.expand(i);
    // like the capture: cwd/home default to the sandbox's
    i["cwd"] = i
        .get("cwd")
        .cloned()
        .unwrap_or(json!(format!("{}/ws", sb.tmp.display())));
    i["home"] = i
        .get("home")
        .cloned()
        .unwrap_or(json!(format!("{}/home", sb.tmp.display())));
    if !sb.setup(&i["fs"]) {
        return None;
    }
    let call = |c: &Value| (c["tool"].as_str().unwrap().to_string(), c["input"].clone());
    Some(match op {
        "suggestions" => {
            let p = permissions(&i, Mode::Default);
            i["calls"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let (t, inp) = call(c);
                    json!({ "tool": t, "input": inp, "suggestions": suggestions_json(&p.suggestions(&t, &inp)) })
                })
                .collect()
        }
        "apply" => {
            let mut p = permissions(&i, Mode::Default);
            let mode = p.apply(&parse_suggestions(&i["suggestions"]));
            let then: Vec<Value> = i["then"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| {
                    let (t, inp) = call(c);
                    let r = verdict_json(sb, &p.check_current(&t, &inp));
                    json!({ "tool": t, "input": inp, "result": r })
                })
                .collect();
            json!({
                "returned_mode": mode.map(Mode::as_str),
                "mode": p.mode().as_str(),
                "allow": p.rules.allow, "deny": p.rules.deny, "ask": p.rules.ask,
                "then": then,
            })
        }
        "check" => i["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                let (t, inp) = call(c);
                let mut by_mode = serde_json::Map::new();
                for m in i["modes"].as_array().unwrap() {
                    let name = m.as_str().unwrap();
                    // an unknown mode behaves like the engine's current one (`default` here)
                    let p = permissions(&i, Mode::parse(name).unwrap_or(Mode::Default));
                    by_mode.insert(
                        name.to_string(),
                        verdict_json(sb, &p.check_current(&t, &inp)),
                    );
                }
                let mut o = json!({ "tool": t, "input": inp, "by_mode": by_mode });
                if i["suggestions"] == json!(true) {
                    o["suggestions"] =
                        suggestions_json(&permissions(&i, Mode::Default).suggestions(&t, &inp));
                }
                o
            })
            .collect(),
        other => panic!("unknown op {other}"),
    })
}

#[test]
fn golden_permissions_match_node() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/permissions");
    let mut cases: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.join("input.json").exists())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no golden cases in {}", dir.display());
    // Cases that differ from Node ON PURPOSE (security fixes), each with its reason. Not a skip:
    // the case still runs, a listed case that now matches Node is an error (stale entry).
    let allowed: Value = serde_json::from_str(
        &fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden-divergence.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let (mut failed, mut skipped, mut diverged) = (Vec::new(), 0, Vec::new());
    for c in &cases {
        let read = |f: &str| -> Value {
            serde_json::from_str(&fs::read_to_string(c.join(f)).unwrap()).unwrap()
        };
        let (input, expected) = (read("input.json"), read("expected.json"));
        match run_case(&input) {
            None => skipped += 1,
            Some(got) if got == expected => {}
            Some(_)
                if allowed
                    .get(c.file_name().unwrap().to_str().unwrap())
                    .is_some() =>
            {
                diverged.push(c.file_name().unwrap().to_string_lossy().into_owned());
            }
            Some(got) => {
                // show only the first differing element of a result list
                let (e, g) = match (expected.as_array(), got.as_array()) {
                    (Some(e), Some(g)) => e
                        .iter()
                        .zip(g)
                        .find(|(a, b)| a != b)
                        .unwrap_or((&expected, &got)),
                    _ => (&expected, &got),
                };
                failed.push(format!(
                    "{}:\n  expected {}\n  got      {}",
                    c.file_name().unwrap().to_string_lossy(),
                    serde_json::to_string(e).unwrap(),
                    serde_json::to_string(g).unwrap()
                ));
            }
        }
    }
    let stale: Vec<&String> = allowed
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| !diverged.contains(k))
        .collect();
    assert!(
        stale.is_empty(),
        "stale golden-divergence entries (case matches Node or is gone): {stale:?}"
    );
    eprintln!(
        "golden permissions: {} cases, {} skipped on this platform, {} diverge on purpose, {} failed",
        cases.len(),
        skipped,
        diverged.len(),
        failed.len()
    );
    // 2 symlink cases + 4 POSIX-path cases; a larger number means the skip rule is hiding cases.
    assert!(
        !cfg!(windows) || skipped <= 6,
        "{skipped} cases skipped on Windows"
    );
    assert!(
        failed.is_empty(),
        "{} golden mismatches:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
