//! Loads every case in tests/golden/providers. Differences from the Node capture are listed in
//! golden_allowlist.json; an unused (stale) entry fails the test, nothing is skipped silently.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use synthesis_providers::anthropic::{self, AnthropicMapper};
use synthesis_providers::http::Mapper;
use synthesis_providers::openai::{self, OpenAiMapper};
use synthesis_providers::{Redaction, Request, anthropic_uses_bearer, normalize_model};

fn read(p: &std::path::Path) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

struct Allow {
    entries: Vec<Value>,
    used: RefCell<HashSet<usize>>,
}

impl Allow {
    fn find(&self, case: &str, kind: &str) -> Option<&Value> {
        let i = self
            .entries
            .iter()
            .position(|e| e["case"] == case && e["kind"] == kind)?;
        self.used.borrow_mut().insert(i);
        Some(&self.entries[i])
    }
}

fn stream_case(case: &str, input: &Value, expected: &Value, allow: &Allow) {
    let req: Request =
        serde_json::from_value(input["req"].clone()).unwrap_or_else(|e| panic!("{case}: req: {e}"));
    let mut want_body = expected["wire_body"].clone();
    if let Some(a) = allow.find(case, "wire_override") {
        *want_body
            .pointer_mut(a["pointer"].as_str().unwrap())
            .unwrap() = a["value"].clone();
    }
    let red = Redaction::new([]);
    let chunks = input["chunks"].as_array().unwrap();
    let (body, events, eof) = if input["provider"] == "anthropic" {
        let mut m = AnthropicMapper::new(red);
        let ev: Vec<_> = chunks
            .iter()
            .flat_map(|c| m.push_value(c.clone()).unwrap())
            .collect();
        (anthropic::build_body(&req), ev, m.finish())
    } else {
        let mut m = OpenAiMapper::new(red);
        let ev: Vec<_> = chunks
            .iter()
            .flat_map(|c| m.push_chunk(c.clone()).unwrap())
            .collect();
        (openai::build_body(&req), ev, m.finish())
    };
    assert_eq!(body, want_body, "{case}: wire body");
    let mut got = serde_json::to_value(&events).unwrap();
    match eof {
        Ok(extra) if extra.is_empty() => {}
        Ok(extra) => {
            let a = allow
                .find(case, "eof_events")
                .unwrap_or_else(|| panic!("{case}: unlisted eof events {extra:?}"));
            assert_eq!(
                serde_json::to_value(&extra).unwrap(),
                a["events"],
                "{case}: eof events"
            );
            // Node ends the stream here without these; compare the rest as recorded.
        }
        Err(_) => {
            allow
                .find(case, "eof_error")
                .unwrap_or_else(|| panic!("{case}: unlisted eof error"));
        }
    }
    let mut want_events = expected["events"].clone();
    if let Some(a) = allow.find(case, "events_override") {
        *want_events
            .pointer_mut(a["pointer"].as_str().unwrap())
            .unwrap() = a["value"].clone();
    }
    assert_eq!(std::mem::take(&mut got), want_events, "{case}: events");
}

#[test]
fn golden_providers() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../../tests/golden/providers");
    let allow = Allow {
        entries: read(&manifest.join("tests/golden_allowlist.json"))["divergences"]
            .as_array()
            .unwrap()
            .clone(),
        used: Default::default(),
    };
    let (mut n, mut reviewed) = (0, 0);
    let mut failed = Vec::new();
    let mut dirs: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    for d in dirs {
        let case = d.file_name().unwrap().to_string_lossy().into_owned();
        let (input, expected) = (read(&d.join("input.json")), read(&d.join("expected.json")));
        if !read(&d.join("meta.json"))["reviewed_by"].is_null() {
            reviewed += 1;
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match input["op"].as_str().unwrap() {
                "stream" => stream_case(&case, &input, &expected, &allow),
                "bearer" => {
                    for (c, e) in input["cases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .zip(expected.as_array().unwrap())
                    {
                        let s = |k: &str| c[k].as_str();
                        let got = anthropic_uses_bearer(
                            s("providerId"),
                            s("authHeader"),
                            s("baseURL"),
                            s("key"),
                        );
                        assert_eq!(json!(got), e["bearer"], "{case}: {c}");
                    }
                }
                "normalizeModels" | "listModels" => {
                    let got: Vec<Value> = input["models"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| serde_json::to_value(normalize_model(m)).unwrap())
                        .collect();
                    assert_eq!(Value::Array(got), expected, "{case}");
                }
                "clientOptions" => {
                    let ignore = allow
                        .find(&case, "ignore_keys")
                        .expect("unlisted clientOptions divergence")["keys"]
                        .clone();
                    for (c, e) in input["cases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .zip(expected.as_array().unwrap())
                    {
                        let mut out = serde_json::Map::new();
                        let s = |k: &str| c[k].as_str().map(String::from);
                        let (key, tok) = (s("apiKey"), s("authToken"));
                        if input["provider"] == "anthropic" {
                            out.insert(
                                "apiKey".into(),
                                if tok.is_some() {
                                    Value::Null
                                } else {
                                    json!(key)
                                },
                            );
                            out.insert("authToken".into(), json!(tok));
                            out.insert(
                                "baseURL".into(),
                                json!(anthropic::base_url(c["baseURL"].as_str())),
                            );
                        } else {
                            if let Some(k) = tok.or(key) {
                                out.insert("apiKey".into(), json!(k));
                            }
                            out.insert(
                                "baseURL".into(),
                                json!(openai::base_url(c["baseURL"].as_str())),
                            );
                        }
                        if c.get("headers").is_some() {
                            out.insert("defaultHeaders".into(), c["headers"].clone());
                        }
                        let mut want = e["out"].clone();
                        for k in ignore.as_array().unwrap() {
                            want.as_object_mut().unwrap().remove(k.as_str().unwrap());
                        }
                        assert_eq!(Value::Object(out), want, "{case}: {c}");
                    }
                }
                op => panic!("{case}: unknown op {op}"),
            }
        }));
        if r.is_err() {
            failed.push(case);
        }
        n += 1;
    }
    assert!(failed.is_empty(), "failing cases: {failed:#?}");
    let stale: Vec<_> = (0..allow.entries.len())
        .filter(|i| !allow.used.borrow().contains(i))
        .collect();
    assert!(stale.is_empty(), "stale allow-list entries: {stale:?}");
    println!(
        "golden providers: {n} cases compared, {reviewed} with meta.reviewed_by set, {} allow-listed divergences",
        allow.entries.len()
    );
    assert_eq!(n, 46);
}
