//! Runs every case in tests/golden/config (captured from the Node reference; see
//! tests/golden/README.md for the placeholder and normalisation rules).

use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use synthesis_config::*;

struct Roots {
    tmp: PathBuf,
    home: PathBuf,
    synthesis_home: PathBuf,
    ws: PathBuf,
}

impl Roots {
    fn expand(&self, s: &str) -> String {
        let p = |p: &Path| p.to_string_lossy().into_owned();
        s.replace("<SYNT_HOME>", &p(&self.synthesis_home))
            .replace("<HOME>", &p(&self.home))
            .replace("<TMP>", &p(&self.tmp))
    }

    /// Longest root first, as the golden README requires.
    fn collapse(&self, v: Value) -> Value {
        let mut roots = [
            (&self.synthesis_home, "<SYNT_HOME>"),
            (&self.home, "<HOME>"),
            (&self.ws, "<TMP>/ws"),
            (&self.tmp, "<TMP>"),
        ];
        roots.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));
        fn walk(v: Value, roots: &[(&PathBuf, &str)]) -> Value {
            match v {
                Value::String(mut s) => {
                    for (p, name) in roots {
                        if s.contains(&*p.to_string_lossy()) {
                            s = s.replace(&*p.to_string_lossy(), name);
                            if cfg!(windows) {
                                s = s.replace('\\', "/"); // fixtures are POSIX-spelled
                            }
                        }
                    }
                    Value::String(s)
                }
                Value::Array(a) => Value::Array(a.into_iter().map(|x| walk(x, roots)).collect()),
                Value::Object(m) => {
                    Value::Object(m.into_iter().map(|(k, x)| (k, walk(x, roots))).collect())
                }
                x => x,
            }
        }
        walk(v, &roots)
    }
}

fn err_json(name: &str, code: Option<&str>, message: String) -> Value {
    let mut e = Map::new();
    e.insert("name".into(), name.into());
    if let Some(c) = code {
        e.insert("code".into(), c.into());
    }
    e.insert("message".into(), message.into());
    json!({ "error": e })
}

fn config_err(e: &ConfigError) -> Value {
    err_json(
        "ConfigValidationError",
        Some(ConfigError::CODE),
        e.to_string(),
    )
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn discovered(v: &Value) -> IndexMap<String, Vec<String>> {
    v.as_object()
        .map(|m| m.iter().map(|(k, x)| (k.clone(), strings(x))).collect())
        .unwrap_or_default()
}

fn run_load(input: &Value, r: &Roots) -> Value {
    for (path, content) in input["files"].as_object().into_iter().flatten() {
        let p = PathBuf::from(r.expand(path));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        match content {
            Value::String(s) => fs::write(&p, s).unwrap(),
            v => fs::write(&p, serde_json::to_string_pretty(v).unwrap()).unwrap(),
        }
    }
    fs::create_dir_all(&r.ws).unwrap();
    let env = Env::from_pairs(
        input["env"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())),
    );
    let synthesis_home = PathBuf::from(r.expand(input["home"].as_str().unwrap()));
    let ctx = LoadContext {
        cwd: &r.ws,
        synthesis_home: &synthesis_home,
        user_home: &r.home,
        env: &env,
    };
    match load_config(&ctx) {
        Ok(c) => json!({ "value": c.to_json() }),
        Err(e) => config_err(&e),
    }
}

fn run_case(input: &Value, r: &Roots) -> Value {
    let cfg = || Config::from_json(&input["config"]);
    match input["op"].as_str().unwrap() {
        "loadConfig" => run_load(input, r),
        "validate" => {
            match validate_versioned_config(&input["raw"], input["file"].as_str().unwrap()) {
                // Node returns { schema, legacy: false } on success.
                Ok(()) => json!({ "value": { "schema": SCHEMA_V1, "legacy": false } }),
                Err(e) => config_err(&e),
            }
        }
        "detectProtocol" => {
            let a = &input["args"];
            let s = |k: &str| a.get(k).and_then(Value::as_str);
            let (t, src) = detect_protocol(DetectInput {
                id: s("id"),
                explicit_type: s("type"),
                npm: s("npm"),
                base_url: s("baseURL"),
                credential: s("credential"),
            });
            json!({ "type": t.as_str(), "source": src.as_str() })
        }
        "bestModelForFamily" => {
            json!({ "value": best_model_for_family(&strings(&input["ids"]), input["family"].as_str()) })
        }
        "parseModel" => match parse_model(input["str"].as_str(), &cfg()) {
            Ok(p) => json!({ "value": { "providerId": p.provider_id, "model": p.model } }),
            Err(e) => err_json("Error", None, e.to_string()),
        },
        "listModels" => {
            let ids = input
                .get("providerIds")
                .filter(|v| !v.is_null())
                .map(strings);
            let l = list_models(&cfg(), ids.as_deref());
            Value::Array(
                l.into_iter()
                    .map(|m| json!({ "value": m.value, "provider": m.provider, "model": m.model }))
                    .collect(),
            )
        }
        "pickDefaultModel" => {
            let v = pick_default_model(
                &cfg(),
                &strings(&input["usableProviderIds"]),
                &discovered(&input["discovered"]),
                input["savedModel"].as_str(),
            );
            json!({ "value": v })
        }
        "resolveModelDefaults" => {
            let explicit: BTreeSet<String> =
                strings(&input["explicitAliases"]).into_iter().collect();
            let d = resolve_model_defaults(
                &cfg(),
                &discovered(&input["discovered"]),
                input["modelExplicit"].as_bool().unwrap_or(false),
                &explicit,
            );
            let mut out = Map::new();
            if let Some(m) = d.model {
                out.insert("model".into(), m.into());
            }
            out.insert(
                "aliases".into(),
                Value::Object(
                    d.aliases
                        .into_iter()
                        .map(|(k, v)| (k, v.map_or(Value::Null, Value::String)))
                        .collect(),
                ),
            );
            out.insert(
                "notes".into(),
                d.notes
                    .iter()
                    .map(|n| json!({ "providerId": n.provider_id, "from": n.from, "to": n.to }))
                    .collect(),
            );
            out.insert(
                "warnings".into(),
                d.warnings
                    .iter()
                    .map(|w| json!({ "providerId": w.provider_id, "model": w.model }))
                    .collect(),
            );
            Value::Object(out)
        }
        op => panic!("unknown golden op {op}"),
    }
}

/// The fixtures store `input.json` with sorted keys, so Node's original key order (and with
/// it the order, and for the 12-cap case the choice, of diagnostics) is unrecoverable.
/// For these cases compare the diagnostics as a multiset (cap case: count and shape only).
fn same_modulo_key_order(case: &Path, actual: &Value, expected: &Value) -> bool {
    let name = case.file_name().unwrap().to_string_lossy();
    if !matches!(
        name.as_ref(),
        "load-error-unknown-top-level-field" | "load-error-more-than-12-problems-truncated"
    ) {
        return false;
    }
    let parts = |v: &Value| -> Vec<String> {
        let m = v["error"]["message"].as_str().unwrap_or_default();
        let mut p: Vec<String> = m
            .split_once(": ")
            .map(|x| x.1)
            .unwrap_or_default()
            .split("; ")
            .map(String::from)
            .collect();
        p.sort();
        p
    };
    let (a, e) = (parts(actual), parts(expected));
    if name.starts_with("load-error-more") {
        a.len() == 12
            && e.len() == 12
            && a.iter()
                .all(|p| p.ends_with(" is not allowed by config.json/v1"))
    } else {
        a == e
    }
}

/// `tests/golden_divergences.txt`: `case-name | reason`. Listed cases must still differ from
/// Node (a stale entry fails), so nothing is skipped silently.
fn divergences() -> Vec<(String, String)> {
    let f = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden_divergences.txt");
    fs::read_to_string(f)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (n, r) = l.split_once('|').expect("case | reason");
            (n.trim().to_string(), r.trim().to_string())
        })
        .collect()
}

#[test]
fn golden_config_cases() {
    let allowed = divergences();
    let mut seen = BTreeSet::new();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/config");
    let mut cases: Vec<_> = fs::read_dir(&dir)
        .expect("tests/golden/config")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty());
    let mut failures = Vec::new();
    for case in &cases {
        let input: Value =
            serde_json::from_slice(&fs::read(case.join("input.json")).unwrap()).unwrap();
        let expected: Value =
            serde_json::from_slice(&fs::read(case.join("expected.json")).unwrap()).unwrap();
        let sandbox = tempfile::tempdir().unwrap();
        // dunce: a verbatim `\\?\` prefix makes the `/` joins in `expand` invalid on Windows
        let tmp = dunce::canonicalize(sandbox.path()).unwrap();
        let r = Roots {
            home: tmp.join("home"),
            synthesis_home: tmp.join("synthesis_home"),
            ws: tmp.join("ws"),
            tmp,
        };
        fs::create_dir_all(&r.home).unwrap();
        fs::create_dir_all(&r.synthesis_home).unwrap();
        let actual = r.collapse(run_case(&input, &r));
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let differs = actual != expected && !same_modulo_key_order(case, &actual, &expected);
        if allowed.iter().any(|(n, _)| *n == name) {
            seen.insert(name.clone());
            if !differs {
                failures.push(format!("{name}: listed as divergent but now matches Node"));
            }
        } else if differs {
            failures.push(format!(
                "{}\n  expected: {expected}\n  actual:   {actual}",
                case.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    for (n, _) in &allowed {
        assert!(seen.contains(n), "allow-list names unknown case {n}");
    }
    assert!(
        failures.is_empty(),
        "{} of {} golden cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
