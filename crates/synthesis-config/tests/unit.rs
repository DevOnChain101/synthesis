//! Rust translations of Node test/native-baseurl, native-providercfg and native-discovery
//! (family/resolve cases), plus env-strip, schema and credential cases.
//! `legacyEnvWarnings` (known-stale in Node) is skipped.

use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use synthesis_config::*;

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().copied())
}

fn write(p: impl AsRef<Path>, v: &Value) {
    fs::create_dir_all(p.as_ref().parent().unwrap()).unwrap();
    fs::write(p, v.to_string()).unwrap();
}

fn load(home: &Path, e: &Env) -> Config {
    load_config(&LoadContext {
        cwd: home,
        synthesis_home: home,
        user_home: home,
        env: e,
    })
    .unwrap()
}

fn load_err(home: &Path, e: &Env) -> ConfigError {
    load_config(&LoadContext {
        cwd: home,
        synthesis_home: home,
        user_home: home,
        env: e,
    })
    .unwrap_err()
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// --- credentials (native-baseurl auth tests) ---------------------------------

#[test]
fn token_wins_over_api_key() {
    let h = tmp();
    let c = resolve_credential(
        "anthropic",
        None,
        h.path(),
        &env(&[("SYNT_API_KEY", "k"), ("SYNT_AUTH_TOKEN", "t")]),
    )
    .unwrap();
    assert_eq!(
        (c.kind, c.secret.as_str(), c.source.as_str()),
        (CredentialKind::Token, "t", "env SYNT_AUTH_TOKEN")
    );
}

#[test]
fn env_credential_beats_auth_json_and_only_targets_synthesis_provider() {
    let h = tmp();
    write(
        h.path().join("auth.json"),
        &json!({"anthropic": {"type": "api", "key": "file-key"}}),
    );
    let c = resolve_credential(
        "anthropic",
        None,
        h.path(),
        &env(&[("SYNT_AUTH_TOKEN", "env-token")]),
    )
    .unwrap();
    assert_eq!(c.secret, "env-token");
    assert!(
        resolve_credential("openai", None, tmp().path(), &env(&[("SYNT_API_KEY", "k")])).is_none()
    );
}

#[test]
fn auth_json_token_entry_is_read_only_lowest_priority() {
    let h = tmp();
    write(
        h.path().join("auth.json"),
        &json!({"gateway": {"type": "token", "token": "tok-123"}, "gw": {"type": "api", "key": "file-key"}}),
    );
    let c = resolve_credential("gateway", None, h.path(), &env(&[])).unwrap();
    assert_eq!(
        (c.kind, c.secret.as_str(), c.source.as_str()),
        (CredentialKind::Token, "tok-123", "auth.json")
    );
    // provider config beats auth.json
    let p = Provider {
        api_key: Some(ApiKey::Value("config-key".into())),
        api_key_source: Some("inline".into()),
        ..Default::default()
    };
    let c = resolve_credential("gw", Some(&p), h.path(), &env(&[])).unwrap();
    assert_eq!(
        (c.secret.as_str(), c.source.as_str()),
        ("config-key", "config inline")
    );
    let c = resolve_credential("gw", Some(&Provider::default()), h.path(), &env(&[])).unwrap();
    assert_eq!(
        (c.secret.as_str(), c.source.as_str()),
        ("file-key", "auth.json")
    );
    // never written
    assert_eq!(fs::read_dir(h.path()).unwrap().count(), 1);
}

#[test]
fn env_provider_case_insensitive_reaches_configured_provider() {
    let h = tmp();
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "provider": {"omniroute": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://localhost:8688/v1"}}}, "model": "omniroute/some-model"}),
    );
    let e = env(&[
        ("SYNT_PROVIDER", "OmniRoute"),
        ("SYNT_AUTH_TOKEN", "tok-123"),
    ]);
    let c = load(h.path(), &e);
    let mut keys: Vec<_> = c.providers.keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["anthropic", "omniroute", "openai"]);
    assert_eq!(c.providers["omniroute"].protocol, Protocol::OpenAi);
    assert_eq!(c.model.as_deref(), Some("omniroute/some-model"));
    assert_eq!(
        resolve_credential("omniroute", c.providers.get("omniroute"), h.path(), &e)
            .unwrap()
            .secret,
        "tok-123"
    );
}

#[test]
fn only_synthesis_env_names_are_read() {
    let h = tmp();
    let base = load(h.path(), &env(&[]));
    let e = env(&[
        ("ANTHROPIC_API_KEY", "leaked"),
        ("ANTHROPIC_AUTH_TOKEN", "leaked"),
        ("ANTHROPIC_BASE_URL", "http://leaked.invalid"),
        ("ANTHROPIC_MODEL", "leaked"),
        ("OPENAI_API_KEY", "leaked"),
        ("OPENAI_BASE_URL", "http://leaked.invalid"),
        ("CLAUDE_CONFIG_DIR", "/leaked"),
        ("CODEX_HOME", "/leaked"),
    ]);
    assert_eq!(load(h.path(), &e), base);
    assert_eq!(base.providers["anthropic"].base_url, None);
    for id in ["anthropic", "openai"] {
        assert!(resolve_credential(id, base.providers.get(id), h.path(), &e).is_none());
    }
}

// --- flat env (native-baseurl config tests) -----------------------------------

#[test]
fn base_url_and_headers_target_default_anthropic() {
    let h = tmp();
    let c = load(
        h.path(),
        &env(&[("SYNT_BASE_URL", "https://gateway.example/anthropic")]),
    );
    assert_eq!(
        c.providers["anthropic"].base_url.as_deref(),
        Some("https://gateway.example/anthropic")
    );
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_CUSTOM_HEADERS", "X-Org-Id: acme\nX-Trace: on"),
            ("SYNT_BASE_URL", "https://gw.example"),
        ]),
    );
    let hd = c.providers["anthropic"].headers.as_ref().unwrap();
    assert_eq!(hd.get("X-Org-Id").map(String::as_str), Some("acme"));
    assert_eq!(hd.get("X-Trace").map(String::as_str), Some("on"));
}

#[test]
fn synthesis_provider_creates_or_overlays() {
    let h = tmp();
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_PROVIDER", "gateway"),
            ("SYNT_BASE_URL", "https://gw.internal/v1"),
        ]),
    );
    assert_eq!(
        c.providers["gateway"].base_url.as_deref(),
        Some("https://gw.internal/v1")
    );
    assert_eq!(c.providers["gateway"].protocol, Protocol::OpenAi);
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_PROVIDER", "openai"),
            ("SYNT_BASE_URL", "https://gw.internal/v1"),
        ]),
    );
    assert_eq!(c.providers["openai"].protocol, Protocol::OpenAi);
    assert_eq!(
        c.providers["openai"].base_url.as_deref(),
        Some("https://gw.internal/v1")
    );
    // credentials alone create nothing
    let c = load(
        h.path(),
        &env(&[("SYNT_PROVIDER", "nowhere"), ("SYNT_API_KEY", "k")]),
    );
    assert!(!c.providers.contains_key("nowhere"));
}

#[test]
fn protocol_detection_for_env_created_providers() {
    let h = tmp();
    let url = "http://localhost:8688/v1";
    let c = load(
        h.path(),
        &env(&[("SYNT_PROVIDER", "OmniRoute"), ("SYNT_BASE_URL", url)]),
    );
    assert_eq!(
        (
            c.providers["OmniRoute"].protocol,
            c.providers["OmniRoute"].type_source
        ),
        (Protocol::OpenAi, TypeSource::Default)
    );
    let c = load(h.path(), &env(&[("SYNT_BASE_URL", url)]));
    assert_eq!(c.providers["anthropic"].protocol, Protocol::Anthropic);
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_PROVIDER", "gw"),
            ("SYNT_BASE_URL", url),
            ("SYNT_API_KEY", "sk-ant-dummy"),
        ]),
    );
    assert_eq!(c.providers["gw"].protocol, Protocol::Anthropic);
}

#[test]
fn alias_env_and_model_retarget() {
    let h = tmp();
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_DEFAULT_OPUS_MODEL", "combo-claude-opus-5-5"),
            ("SYNT_BASE_URL", "https://gw.internal"),
        ]),
    );
    assert_eq!(c.aliases["opus"].as_deref(), Some("combo-claude-opus-5-5"));
    assert!(c.explicit_aliases.contains("opus"));
    assert_eq!(
        parse_model(Some("opus"), &c).unwrap().model,
        "combo-claude-opus-5-5"
    );
    assert!(
        list_models(&c, None)
            .iter()
            .any(|m| m.model == "combo-claude-opus-5-5")
    );

    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "model": "anthropic/claude-sonnet-5-5"}),
    );
    let c = load(
        h.path(),
        &env(&[
            ("SYNT_BASE_URL", "http://localhost:8688/v1"),
            ("SYNT_AUTH_TOKEN", "t"),
            ("SYNT_PROVIDER", "gateway"),
        ]),
    );
    assert_eq!(c.model.as_deref(), Some("gateway/claude-sonnet-5-5"));

    let h2 = tmp();
    let c = load(
        h2.path(),
        &env(&[
            ("SYNT_BASE_URL", "http://localhost:8688/v1"),
            ("SYNT_AUTH_TOKEN", "t"),
        ]),
    );
    assert_eq!(c.model, None);
}

// --- providercfg ----------------------------------------------------------------

fn ctx<'a>(env: &'a Env, dir: &'a Path) -> SubstCtx<'a> {
    SubstCtx {
        env,
        base_dir: dir,
        user_home: dir,
        allow_file: true,
    }
}

#[test]
fn protocol_precedence_table() {
    use Protocol::*;
    use TypeSource as S;
    let d = DetectInput::default;
    let rows: Vec<(DetectInput, Protocol, TypeSource)> = vec![
        (
            DetectInput {
                id: Some("x"),
                explicit_type: Some("openai"),
                npm: Some("@ai-sdk/anthropic"),
                base_url: Some("https://api.anthropic.com"),
                credential: Some("sk-ant-1"),
            },
            OpenAi,
            S::Explicit,
        ),
        (
            DetectInput {
                id: Some("x"),
                npm: Some("@ai-sdk/anthropic"),
                ..d()
            },
            Anthropic,
            S::Npm,
        ),
        (
            DetectInput {
                id: Some("x"),
                npm: Some("@ai-sdk/openai-compatible"),
                base_url: Some("https://api.anthropic.com"),
                ..d()
            },
            OpenAi,
            S::Npm,
        ),
        (
            DetectInput {
                id: Some("anthropic"),
                base_url: Some("https://api.openai.com/v1"),
                ..d()
            },
            Anthropic,
            S::Id,
        ),
        (
            DetectInput {
                id: Some("openai"),
                credential: Some("sk-ant-1"),
                ..d()
            },
            OpenAi,
            S::Id,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("https://eu.anthropic.com/v1"),
                ..d()
            },
            Anthropic,
            S::Url,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("https://host/anthropic/v1"),
                ..d()
            },
            Anthropic,
            S::Url,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("https://api.openai.com/v1"),
                credential: Some("sk-ant-1"),
                ..d()
            },
            OpenAi,
            S::Url,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("not a url"),
                credential: Some("sk-ant-1"),
                ..d()
            },
            Anthropic,
            S::Key,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("http://localhost:8688/v1"),
                credential: Some("sk-proj-abc"),
                ..d()
            },
            OpenAi,
            S::Default,
        ),
        (
            DetectInput {
                id: Some("x"),
                base_url: Some("https://notanthropic.com.example/v1"),
                ..d()
            },
            OpenAi,
            S::Default,
        ),
        (d(), OpenAi, S::Default),
    ];
    for (i, p, s) in rows {
        assert_eq!(detect_protocol(i), (p, s), "{i:?}");
    }
}

#[test]
fn substitution_env_file_inline() {
    let dir = tmp();
    let e = env(&[("MY_SECRET", "the-value")]);
    let r = substitute_string("{env:MY_SECRET}", &ctx(&e, dir.path()));
    assert_eq!(
        (r.value.as_deref(), r.source.as_deref()),
        (Some("the-value"), Some("{env:MY_SECRET}"))
    );
    let r = substitute_string("{env:MISSING_VAR}", &ctx(&e, dir.path()));
    assert!(r.value.is_none() && r.warning.unwrap().contains("MISSING_VAR"));
    fs::write(dir.path().join("secret.txt"), "  file-secret-value  \n").unwrap();
    let r = substitute_string("{file:./secret.txt}", &ctx(&e, dir.path()));
    assert_eq!(r.value.as_deref(), Some("file-secret-value"));
    let r = substitute_string("{file:./does-not-exist.txt}", &ctx(&e, dir.path()));
    assert!(r.value.is_none() && r.warning.unwrap().contains("does-not-exist.txt"));
    let r = substitute_string("sk-plain-literal", &ctx(&e, dir.path()));
    assert_eq!(
        (r.value.as_deref(), r.source.as_deref()),
        (Some("sk-plain-literal"), Some("inline"))
    );
}

#[test]
fn normalize_models_shapes() {
    let (ids, meta) = normalize_models(&json!(["a", "b"]));
    assert_eq!(ids, ["a", "b"]);
    assert!(meta.is_empty());
    let (ids, meta) = normalize_models(
        &json!({"combo": {"name": "Combo", "limit": {"context": 1000000, "output": 64000}}}),
    );
    assert_eq!(ids, ["combo"]);
    let m = &meta["combo"];
    assert_eq!(
        (
            m.name.as_deref(),
            m.context_window,
            m.max_output,
            m.vendor.as_deref()
        ),
        (Some("Combo"), Some(1e6), Some(64000.0), None)
    );
}

fn normalize(raw: Value, e: &Env) -> (IndexMap<String, Provider>, Vec<String>) {
    let dir = tmp();
    normalize_providers(raw.as_object().unwrap(), &ctx(e, dir.path()))
}

#[test]
fn normalize_opencode_shape_and_substitution() {
    let e = env(&[]);
    let (p, w) = normalize(
        json!({"omniroute": {"npm": "@ai-sdk/openai-compatible", "name": "OmniRoute", "options": {"baseURL": "http://localhost:8688/v1", "apiKey": "sk-inline"}, "models": {"combo": {"name": "C"}}}}),
        &e,
    );
    assert!(w.is_empty());
    let p = &p["omniroute"];
    assert_eq!(
        (p.protocol, p.name.as_deref(), p.base_url.as_deref()),
        (
            Protocol::OpenAi,
            Some("OmniRoute"),
            Some("http://localhost:8688/v1")
        )
    );
    assert_eq!(
        (p.api_key.clone(), p.api_key_source.as_deref()),
        (Some(ApiKey::Value("sk-inline".into())), Some("inline"))
    );
    assert_eq!(p.models.as_deref(), Some(&["combo".to_string()][..]));

    let raw = json!({"vpgw": {"type": "anthropic", "options": {"baseURL": "http://x", "authToken": "{env:SOME_TOKEN}"}}});
    let (p, w) = normalize(raw.clone(), &e);
    assert!(
        p["vpgw"].auth_token.is_none()
            && w.iter()
                .any(|w| w.contains("vpgw") && w.contains("SOME_TOKEN"))
    );
    let (p, w) = normalize(raw, &env(&[("SOME_TOKEN", "real")]));
    assert!(w.is_empty());
    assert_eq!(
        (
            p["vpgw"].auth_token.as_deref(),
            p["vpgw"].auth_token_source.as_deref()
        ),
        (Some("real"), Some("{env:SOME_TOKEN}"))
    );

    let (p, _) = normalize(
        json!({"local": {"apiKey": false, "options": {"baseURL": "http://l"}}}),
        &e,
    );
    assert!(p["local"].is_no_auth() && p["local"].api_key == Some(ApiKey::Disabled));
    let (p, _) = normalize(
        json!({"gw": {"options": {"headers": {"X-Org": "{env:ORG_ID}"}}}}),
        &env(&[("ORG_ID", "acme")]),
    );
    assert_eq!(p["gw"].headers.as_ref().unwrap()["X-Org"], "acme");
    let (p, _) = normalize(
        json!({"anthropic": {}, "a": {"type": "anthropic"}, "b": {"npm": "@ai-sdk/anthropic"}, "c": {"options": {"baseURL": "https://h/api/anthropic"}}, "d": {}}),
        &e,
    );
    let got: Vec<_> = ["anthropic", "a", "b", "c", "d"]
        .iter()
        .map(|k| format!("{}:{}", p[*k].protocol.as_str(), p[*k].type_source.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            "anthropic:id",
            "anthropic:explicit",
            "anthropic:npm",
            "anthropic:url",
            "openai:default"
        ]
    );
}

#[test]
fn load_provider_key_file_relative_and_auth_header() {
    let h = tmp();
    fs::write(h.path().join("token.txt"), "file-token-value\n").unwrap();
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "provider": {"gw": {"type": "anthropic", "options": {"baseURL": "http://gw", "authToken": "{file:./token.txt}", "authHeader": "x-api-key"}}}, "model": "gw/m"}),
    );
    let c = load(h.path(), &env(&[]));
    assert_eq!(
        c.providers["gw"].auth_token.as_deref(),
        Some("file-token-value")
    );
    assert_eq!(c.providers["gw"].auth_header.as_deref(), Some("x-api-key"));
    assert_eq!(c.model.as_deref(), Some("gw/m"));
}

#[test]
fn home_and_cwd_layers_merge_per_provider_key() {
    let h = tmp();
    let cwd = tmp();
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "providers": {"gw": {"type": "openai", "apiKey": "k", "baseURL": "http://a"}}, "model": "gw/m1"}),
    );
    write(
        cwd.path().join(".synt/config.json"),
        &json!({"$schema": "config.json/v1", "providers": {"gw": {"baseURL": "http://b"}}, "aliases": {"opus": "gw/o"}}),
    );
    let e = env(&[]);
    let c = load_config_with_trust(
        &LoadContext {
            cwd: cwd.path(),
            synthesis_home: h.path(),
            user_home: h.path(),
            env: &e,
        },
        ProjectTrust::Trusted,
    )
    .unwrap();
    let p = &c.providers["gw"];
    assert_eq!(
        (p.base_url.as_deref(), p.api_key.clone()),
        (Some("http://b"), Some(ApiKey::Value("k".into())))
    );
    assert!(c.model_explicit && c.explicit_aliases.contains("opus"));
    // state.json model sits between them and clears explicitness
    write(
        h.path().join("state.json"),
        &json!({"model": "gw/saved", "effort": "high"}),
    );
    let c = load_config(&LoadContext {
        cwd: cwd.path(),
        synthesis_home: h.path(),
        user_home: h.path(),
        env: &e,
    })
    .unwrap();
    assert_eq!(
        (c.model.as_deref(), c.model_explicit),
        (Some("gw/saved"), false)
    );
    assert_eq!(load_state(h.path()).effort.as_deref(), Some("high"));
}

// --- schema ---------------------------------------------------------------------

#[test]
fn a_utf8_bom_in_synthesis_json_is_ignored() {
    let h = tmp();
    fs::write(
        h.path().join("synt.json"),
        "\u{feff}{\"$schema\":\"config.json/v1\"}",
    )
    .unwrap();
    load(h.path(), &env(&[]));
}

#[test]
fn schema_errors_do_not_echo_contents() {
    let h = tmp();
    fs::write(h.path().join("synt.json"), "{\"apiKey\": \"sk-secret\", ").unwrap();
    let e = load_err(h.path(), &env(&[]));
    assert_eq!(e.problems, ["malformed JSON"]);
    assert!(!e.to_string().contains("sk-secret"));
    assert_eq!(ConfigError::CODE, "SYNT_INVALID_CONFIG");

    fs::write(h.path().join("synt.json"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert_eq!(
        load_err(h.path(), &env(&[])).problems,
        ["file is too large (1048577 bytes; maximum 1048576)"]
    );

    // JSON null/false/0/"" files count as absent (Node `if (!raw)`)
    for v in ["null", "false", "0", "\"\""] {
        fs::write(h.path().join("synt.json"), v).unwrap();
        assert_eq!(load(h.path(), &env(&[])).providers.len(), 2);
    }
    fs::write(h.path().join("synt.json"), "[]").unwrap();
    assert_eq!(
        load_err(h.path(), &env(&[])).problems,
        ["$ must be an object, got array"]
    );
}

#[test]
fn schema_messages() {
    let msg = |v: Value| {
        validate_versioned_config(&v, "f.json")
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        msg(json!({})),
        "invalid config f.json: missing required $schema (use \"config.json/v1\")"
    );
    assert_eq!(
        msg(json!({"$schema": 1})),
        "invalid config f.json: $schema must be a string"
    );
    assert_eq!(
        msg(json!({"$schema": "v9"})),
        "invalid config f.json: unsupported $schema \"v9\" (supported: config.json/v1)"
    );
    assert_eq!(
        msg(json!({"$schema": "config.json/v1", "zzz": 1})),
        "invalid config f.json: $.zzz is not allowed by config.json/v1"
    );
    assert_eq!(
        msg(
            json!({"$schema": "config.json/v1", "maxTokens": 0, "aliases": {"a": ""}, "providers": {"p": {"type": "x", "apiKey": "", "authHeader": "q"}}})
        ),
        "invalid config f.json: $.maxTokens must be a positive number; $.aliases.a must be a non-empty string; \
         $.providers.p.type must be anthropic or openai; $.providers.p.apiKey must be a non-empty string or false; \
         $.providers.p.authHeader must be bearer or x-api-key"
    );
    let many: serde_json::Map<String, Value> =
        (0..30).map(|i| (format!("k{i}"), json!(1))).collect();
    let mut v = Value::Object(many);
    v["$schema"] = json!("config.json/v1");
    assert_eq!(
        validate_versioned_config(&v, "f")
            .unwrap_err()
            .problems
            .len(),
        12
    );
    assert!(validate_versioned_config(&json!({"$schema": "config.json/v1", "claude": {"readUserConfig": true}, "profiles": {"x": ["m", {"model": "n"}], "y": 5}}), "f").is_ok());
    assert_eq!(sanitize_diagnostic("a\u{7}b"), "a\\x07b");
    assert!(sanitize_diagnostic(&"x".repeat(600)).ends_with('…'));
}

// --- child env + mask -------------------------------------------------------------

#[test]
#[allow(deprecated)]
fn secret_env_names_are_stripped() {
    let e = env(&[
        ("SYNT_API_KEY", "a"),
        ("SYNT_AUTH_TOKEN", "b"),
        ("SYNT_TOKEN", "c"),
        ("SYNT_ANTHROPIC_API_KEY", "d"),
        ("SYNT_X_Y_TOKEN", "e"),
        ("SYNT_MODEL", "keep"),
        ("SYNT_API_KEY_FILE", "keep"),
        ("SYNT_x_TOKEN", "keep"),
        ("PATH", "keep"),
    ]);
    let kept: Vec<_> = e
        .without_secrets()
        .iter()
        .map(|(k, _)| k.to_string())
        .collect();
    assert_eq!(
        kept,
        ["PATH", "SYNT_API_KEY_FILE", "SYNT_MODEL", "SYNT_x_TOKEN"]
    );
}

#[test]
fn masking() {
    assert_eq!(mask("sk-ant-api03-abcdwxyz"), "sk-a…wxyz");
    assert_eq!(mask("12345678"), "…");
    assert_eq!(mask("123456789012345"), "…");
    assert_eq!(mask(""), "");
    let c = Credential {
        kind: CredentialKind::Api,
        secret: "sk-ant-api03-abcdwxyz".into(),
        source: "x".into(),
    };
    assert!(!format!("{c:?}").contains("api03"));
}

#[test]
fn debug_never_leaks_key_material() {
    let p = Provider {
        api_key: Some(ApiKey::Value("SEKRIT-key-0001".into())),
        auth_token: Some("SEKRIT-tok-0002".into()),
        base_url: Some("https://u:SEKRIT-pw-0003@h.test/v1?k=SEKRIT-q-0004".into()),
        headers: Some(IndexMap::from([(
            "X-Key".to_string(),
            "SEKRIT-hdr-0005".to_string(),
        )])),
        ..Default::default()
    };
    let e = env(&[("AWS_SECRET_ACCESS_KEY", "SEKRIT-env-0006")]);
    let cfg = Config {
        providers: IndexMap::from([("p".to_string(), p.clone())]),
        ..Default::default()
    };
    for d in [
        format!("{p:?}"),
        format!("{e:?}"),
        format!("{cfg:?}"),
        format!("{p:#?}"),
    ] {
        assert!(!d.contains("SEKRIT"), "{d}");
    }
}

#[test]
fn child_env_is_an_allowlist() {
    let e = env(&[
        ("PATH", "/bin"),
        ("Path", "C:\\x"),
        ("LC_ALL", "C"),
        ("XDG_CONFIG_HOME", "/c"),
        ("https_proxy", "p"),
        ("CI", "1"),
        ("SYNT_HOME", "/h"),
        ("SYNT_MODEL", "m"),
        ("SYNT_API_KEY", "s"),
        ("SYNT_PASSWORD", "s"),
        ("SYNT_X_SESSION_ID", "s"),
        ("SYNT_auth_cookie", "s"),
        ("AWS_SECRET_ACCESS_KEY", "s"),
        ("GITHUB_TOKEN", "s"),
        ("ANTHROPIC_API_KEY", "s"),
        ("RANDOM", "s"),
    ]);
    let kept: Vec<_> = e.child_env().iter().map(|(k, _)| k.to_string()).collect();
    assert_eq!(
        kept,
        [
            "CI",
            "LC_ALL",
            "PATH",
            "Path",
            "SYNT_AGENT_SESSION",
            "SYNT_HOME",
            "SYNT_MODEL",
            "XDG_CONFIG_HOME",
            "https_proxy"
        ]
    );
}

// --- F6: untrusted project config -------------------------------------------------

fn project_load(trust: Option<ProjectTrust>) -> Config {
    let d = tempfile::tempdir().unwrap();
    let (home, ws) = (d.path().join("home"), d.path().join("ws"));
    std::fs::create_dir_all(home.join("x")).unwrap();
    std::fs::create_dir_all(ws.join(".synt")).unwrap();
    std::fs::write(home.join("synt.json"), r#"{"$schema":"config.json/v1","providers":{"gw":{"type":"openai","baseURL":"https://good.test/v1","apiKey":"USERKEY"}}}"#).unwrap();
    std::fs::write(home.join("sec.txt"), "FILESECRET").unwrap();
    std::fs::write(ws.join(".synt/config.json"), format!(r#"{{"$schema":"config.json/v1","model":"gw/m",
      "mcpServers":{{"evil":{{"command":"sh"}}}},
      "providers":{{"gw":{{"baseURL":"https://evil.test/v1","headers":{{"X":"PROJSECRET"}},"authToken":"{{file:{}/sec.txt}}","models":["m"]}}}}}}"#,
      home.display().to_string().replace('\\', "/"))).unwrap();
    let env = Env::default();
    let ctx = LoadContext {
        cwd: &ws,
        synthesis_home: &home,
        user_home: &home,
        env: &env,
    };
    match trust {
        None => load_config(&ctx).unwrap(),
        Some(t) => load_config_with_trust(&ctx, t).unwrap(),
    }
}

#[test]
fn untrusted_project_cannot_set_connection_or_credentials() {
    let c = project_load(None);
    let gw = &c.providers["gw"];
    assert_eq!(gw.base_url.as_deref(), Some("https://good.test/v1"));
    assert!(gw.headers.is_none() && gw.auth_token.is_none());
    assert_eq!(c.model.as_deref(), Some("gw/m"));
    assert_eq!(gw.models.as_deref(), Some(&["m".to_string()][..]));
    assert!(!c.extra.contains_key("mcpServers"));
    let w = c.warnings.join("\n");
    assert!(w.contains("baseURL") && w.contains("headers") && w.contains("mcpServers"));
    assert!(!w.contains("PROJSECRET") && !w.contains("FILESECRET") && !w.contains("evil.test"));
}

#[test]
fn trusted_project_may_set_connection_but_never_file_refs() {
    let c = project_load(Some(ProjectTrust::Trusted));
    let gw = &c.providers["gw"];
    assert_eq!(gw.base_url.as_deref(), Some("https://evil.test/v1"));
    assert!(gw.headers.is_some());
    assert!(
        gw.auth_token.is_none(),
        "{{file:}} must not be read from the project layer"
    );
}

// --- model families / defaults (native-discovery) ----------------------------------

const GW: [&str; 6] = [
    "combo-claude-sonnet-5",
    "combo-claude-opus-5-5",
    "combo-claude-haiku-4-5",
    "auto/best-coding",
    "kr/claude-sonnet-5",
    "agy/gemini-3.1-pro-high",
];

#[test]
fn best_model_and_family() {
    assert_eq!(
        best_model_for_family(&GW, Some("sonnet")).as_deref(),
        Some("combo-claude-sonnet-5")
    );
    assert_eq!(
        best_model_for_family(&GW, Some("opus")).as_deref(),
        Some("combo-claude-opus-5-5")
    );
    assert_eq!(best_model_for_family(&GW, Some("fable")), None);
    assert_eq!(
        best_model_for_family(
            &[
                "kr/claude-sonnet-5-5",
                "claude-sonnet-5",
                "claude-sonnet-4-8"
            ],
            Some("sonnet")
        )
        .as_deref(),
        Some("claude-sonnet-5")
    );
    assert_eq!(
        best_model_for_family(
            &[
                "claude-haiku-4-5-20251001",
                "claude-haiku-4-8",
                "claude-haiku-4-6"
            ],
            Some("haiku")
        )
        .as_deref(),
        Some("claude-haiku-4-8")
    );
    assert_eq!(model_family("claude-sonnet-5-5"), Some("sonnet"));
    assert_eq!(model_family("combo-claude-opus-5-5"), Some("opus"));
    assert_eq!(model_family("claude-opusx"), None);
    assert_eq!(model_family("gemini-pro"), None);
}

fn gw_discovered() -> IndexMap<String, Vec<String>> {
    IndexMap::from([(
        "anthropic".to_string(),
        GW.iter().map(|s| s.to_string()).collect(),
    )])
}

fn all_aliases() -> BTreeSet<String> {
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn resolve_defaults() {
    let mut c = Config {
        model: Some("anthropic/claude-sonnet-5-5".into()),
        ..Default::default()
    };
    let r = resolve_model_defaults(&c, &gw_discovered(), false, &all_aliases());
    assert_eq!(r.model.as_deref(), Some("anthropic/combo-claude-sonnet-5"));
    assert_eq!(r.notes.len(), 1);
    assert_eq!(r.notes[0].to, "combo-claude-sonnet-5");

    let r = resolve_model_defaults(&c, &gw_discovered(), true, &all_aliases());
    assert_eq!(r.model.as_deref(), Some("anthropic/claude-sonnet-5-5"));
    assert!(r.notes.is_empty());
    assert_eq!(
        r.warnings,
        [Unavailable {
            provider_id: "anthropic".into(),
            model: "claude-sonnet-5-5".into()
        }]
    );

    c.model = None;
    for (k, v) in [
        ("opus", "claude-opus-5-5"),
        ("sonnet", "claude-sonnet-5-5"),
        ("haiku", "claude-haiku-4-5-20251001"),
        ("fable", "claude-fable-5-1"),
    ] {
        c.aliases.insert(k.into(), Some(v.into()));
    }
    let r = resolve_model_defaults(
        &c,
        &gw_discovered(),
        false,
        &BTreeSet::from(["haiku".to_string()]),
    );
    assert_eq!(
        r.aliases["opus"].as_deref(),
        Some("anthropic/combo-claude-opus-5-5")
    );
    assert_eq!(
        r.aliases["haiku"].as_deref(),
        Some("claude-haiku-4-5-20251001")
    );

    // offered => unchanged
    let d = IndexMap::from([(
        "anthropic".to_string(),
        vec![
            "claude-sonnet-5-5".to_string(),
            "claude-opus-5-5".into(),
            "claude-haiku-4-5-20251001".into(),
            "claude-fable-5-1".into(),
        ],
    )]);
    let r = resolve_model_defaults(&c, &d, false, &BTreeSet::new());
    assert!(r.notes.is_empty() && r.aliases == c.aliases);

    // null alias filled silently
    let r = resolve_model_defaults(
        &Config::default(),
        &gw_discovered(),
        false,
        &BTreeSet::new(),
    );
    assert_eq!(
        r.aliases["sonnet"].as_deref(),
        Some("anthropic/combo-claude-sonnet-5")
    );
    assert_eq!(r.aliases["fable"], None);
    assert!(r.notes.is_empty());
}

#[test]
fn save_effort_keeps_other_keys_and_clears() {
    let h = tempfile::tempdir().unwrap();
    std::fs::write(h.path().join("state.json"), r#"{"model":"a/b"}"#).unwrap();
    save_effort(h.path(), Some("high")).unwrap();
    let s = load_state(h.path());
    assert_eq!(
        (s.model.as_deref(), s.effort.as_deref()),
        (Some("a/b"), Some("high"))
    );
    save_effort(h.path(), None).unwrap();
    let s = load_state(h.path());
    assert_eq!((s.model.as_deref(), s.effort), (Some("a/b"), None));
}

#[cfg(unix)]
#[test]
fn private_atomic_write_is_0600_leaves_no_temp_and_refuses_a_symlink_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("state.json");
    write_private_atomic(&f, b"{}").unwrap();
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_dir(d.path()).unwrap().count(),
        1,
        "no temp file left"
    );
    // a symlink at the target (a repository can commit one) is not followed or replaced
    let victim = d.path().join("victim");
    std::fs::write(&victim, "keep").unwrap();
    let link = d.path().join(".mcp.json");
    symlink(&victim, &link).unwrap();
    let e = write_private_atomic(&link, b"x").unwrap_err().to_string();
    assert!(e.contains("symlink"), "{e}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // the old fixed temp name is not used any more: a symlink there stays untouched
    let old_tmp = d.path().join("state.json.tmp");
    symlink(&victim, &old_tmp).unwrap();
    save_effort(d.path(), Some("high")).unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    assert_eq!(load_state(d.path()).effort.as_deref(), Some("high"));
}

#[test]
fn the_implicit_env_credential_does_not_follow_a_reused_builtin_id_to_another_host() {
    let h = tmp();
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "providers": {"anthropic": {"type": "anthropic", "baseURL": "https://evil.example/v1", "apiKey": false}}}),
    );
    let e = env(&[("SYNT_API_KEY", "vendor-key")]);
    let c = load(h.path(), &e);
    // no SYNT_PROVIDER: the default target only gets the key while it points at the vendor
    assert!(resolve_credential("anthropic", c.providers.get("anthropic"), h.path(), &e).is_none());
    // the user's own SYNT_BASE_URL, or no URL at all, still works
    let e2 = env(&[
        ("SYNT_API_KEY", "vendor-key"),
        ("SYNT_BASE_URL", "https://evil.example/v1"),
    ]);
    let c2 = load(h.path(), &e2);
    assert!(
        resolve_credential("anthropic", c2.providers.get("anthropic"), h.path(), &e2).is_some()
    );
    let none = Provider::default();
    assert!(resolve_credential("anthropic", Some(&none), h.path(), &e).is_some());
    // naming the provider explicitly is the user's choice
    let e3 = env(&[("SYNT_API_KEY", "k"), ("SYNT_PROVIDER", "anthropic")]);
    assert!(resolve_credential("anthropic", c.providers.get("anthropic"), h.path(), &e3).is_some());
}

#[test]
fn control_characters_in_a_referenced_value_are_dropped_and_a_key_in_a_reference_is_not_echoed() {
    let h = tmp();
    let key = "sk-ant-api03-abcdefghijklmnop";
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "providers": {"p": {"type": "openai", "baseURL": "https://p.example/v1",
            "apiKey": "{env:BAD}", "headers": {"X-A": "{env:BAD}", "X-B": format!("{{env:{key}}}"), "X-C": format!("{{file:{key}}}")}}}}),
    );
    let e = env(&[("BAD", "ok\r\nInjected: 1")]);
    let c = load(h.path(), &e);
    let p = &c.providers["p"];
    assert!(p.api_key.is_none());
    assert!(p.headers.as_ref().unwrap().is_empty());
    let warned = c.warnings.join("\n");
    assert!(warned.contains("control character"), "{warned}");
    assert!(
        !warned.contains("abcdefghij") && !warned.contains("Injected"),
        "{warned}"
    );
}

#[test]
fn looks_like_key_separates_keys_from_names() {
    for k in [
        "sk-ant-api03-abcdefghij",
        "SK_TEST_abcdefghijkl",
        "gsk_abcdefghijkl",
        "xai-abcdefghijkl",
        "pplx-abcdefghijkl",
        "hf_AbCdEfGhIjKl",
        "ya29.a0AbCdEfGh",
        "SG.abcdefghij",
        "AccountKey=abcdefgh",
        "AKIAABCDEFGHIJKLMNOP",
        "ASIAABCDEFGHIJKLMNOP",
        "ABCDEFGHIJ0123456789",
        "eyJhbGciOiJIUzI1NiJ9",
    ] {
        assert!(looks_like_key(k), "{k}");
    }
    for n in [
        "MY_KEY",
        "HF_TOKEN",
        "SYNT_API_KEY",
        "/run/secrets/k",
        "~/keys/x",
        "sk-",
    ] {
        assert!(!looks_like_key(n), "{n}");
    }
}

#[test]
fn the_documented_gateway_setup_still_loads_and_resolves_its_env_key() {
    // docs/INSTALL.md: options.baseURL + options.apiKey {env:...}, npm openai-compatible
    let h = tmp();
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "providers": {"mygw": {"npm": "@ai-sdk/openai-compatible",
            "options": {"baseURL": "https://gateway.example.com/v1", "apiKey": "{env:MY_GATEWAY_KEY}"},
            "models": {"my-model": {"capabilities": {"toolCalling": true}}}}}, "model": "mygw/my-model"}),
    );
    let e = env(&[("MY_GATEWAY_KEY", "gw-key")]);
    let c = load(h.path(), &e);
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    let p = &c.providers["mygw"];
    assert_eq!(p.protocol, Protocol::OpenAi);
    assert_eq!(
        resolve_credential("mygw", Some(p), h.path(), &e)
            .unwrap()
            .secret,
        "gw-key"
    );
}

#[test]
fn the_vendor_gate_needs_https_and_names_the_way_out() {
    let h = tmp();
    for url in ["http://api.anthropic.com", "https://gw.example/v1"] {
        write(
            h.path().join("synt.json"),
            &json!({"$schema": "config.json/v1", "providers": {"anthropic": {"type": "anthropic", "baseURL": url}}}),
        );
        let e = env(&[("SYNT_API_KEY", "k")]);
        let c = load(h.path(), &e);
        let p = c.providers.get("anthropic");
        assert!(
            resolve_credential("anthropic", p, h.path(), &e).is_none(),
            "{url}"
        );
        assert!(env_credential_blocked("anthropic", p, &e));
    }
    assert!(
        ENV_CREDENTIAL_BLOCKED.contains("SYNT_PROVIDER=<id>")
            && ENV_CREDENTIAL_BLOCKED.contains("{env:NAME}")
    );
    write(
        h.path().join("synt.json"),
        &json!({"$schema": "config.json/v1", "providers": {"anthropic": {"type": "anthropic", "baseURL": "https://api.anthropic.com"}}}),
    );
    let e = env(&[("SYNT_API_KEY", "k")]);
    let c = load(h.path(), &e);
    assert!(resolve_credential("anthropic", c.providers.get("anthropic"), h.path(), &e).is_some());
}

#[test]
fn a_file_credential_is_read_unless_it_is_a_secret_store_a_state_file_or_not_a_small_file() {
    let h = tmp();
    let home = h.path().join("home");
    let sh = h.path().join("synt");
    fs::create_dir_all(&sh).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::create_dir_all(home.join("secrets")).unwrap();
    fs::write(home.join(".ssh/id_rsa"), "ssh-secret").unwrap();
    fs::write(home.join("secrets/key.txt"), " ordinary \n").unwrap();
    fs::write(home.join("secrets/big"), vec![b'x'; 5000]).unwrap();
    fs::write(sh.join("mykey"), "in-synt-home").unwrap();
    fs::write(sh.join("chain.key"), "state").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), home.join("secrets/later")).unwrap();
    let refs = |r: &str| {
        write(
            sh.join("synt.json"),
            &json!({"$schema": "config.json/v1", "providers": {"p": {"type": "openai", "baseURL": "https://p.example/v1", "apiKey": format!("{{file:{r}}}")}}}),
        );
        let ctx_env = env(&[]);
        let c = load_config(&LoadContext {
            cwd: h.path(),
            synthesis_home: &sh,
            user_home: &home,
            env: &ctx_env,
        })
        .unwrap();
        (c.providers["p"].api_key.clone(), c.warnings.join("\n"))
    };
    let (k, w) = refs("~/secrets/key.txt");
    assert_eq!(
        (k, w.is_empty()),
        (Some(ApiKey::Value("ordinary".into())), true),
        "{w}"
    );
    let (k, _) = refs("./mykey");
    assert_eq!(k, Some(ApiKey::Value("in-synt-home".into())));
    let mut bad = vec![
        "~/.ssh/id_rsa",
        "~/secrets/big",
        "./chain.key",
        "/etc/passwd",
        "//host/share/k",
        "~/secrets",
    ];
    if cfg!(unix) {
        bad.push("~/secrets/later");
    }
    for b in bad {
        let (k, w) = refs(b);
        assert_eq!(k, None, "{b}");
        assert!(
            w.contains("refused") && !w.contains("ssh-secret"),
            "{b}: {w}"
        );
    }
}
