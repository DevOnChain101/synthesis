//! The model picker's data: the config's models plus the ones a refresh found, each with what the
//! cache knows (`synt models` shows context window, max output, vision; tiers and a price are added
//! when known). Nothing is guessed: an unknown value shows as `?`.

use std::path::Path;

use synthesis_app::models as app;
use synthesis_config::{Env, ProjectTrust, list_models};
use synthesis_harness::sanitize_for_display as san;
use synthesis_providers::modelinfo::ModelFacts;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    /// `provider/model`
    pub value: String,
    pub detail: String,
}

fn k(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(100_000) {
        format!("{}m", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

/// `ctx 200k · out 64k · vision yes · effort low|high · $3/$15`: only the fields that are known (none: empty).
pub fn detail(f: Option<&ModelFacts>, price: Option<(f64, f64)>) -> String {
    let vision = f
        .and_then(|f| f.vision)
        .map(|v| if v { "yes" } else { "no" });
    let tiers = f
        .map(|f| f.effort_tiers.join("|"))
        .filter(|t| !t.is_empty());
    [
        f.and_then(|f| f.context_window)
            .map(|n| format!("ctx {}", k(n))),
        f.and_then(|f| f.max_output)
            .map(|n| format!("out {}", k(n))),
        vision.map(|v| format!("vision {v}")),
        tiers.map(|t| format!("effort {t}")),
        price.map(|(i, o)| format!("${i}/${o}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ")
}

/// `1M (the provider's model list (cache))` or why the window is unknown and how to set it.
pub fn window_note(env: &Env, project: &Path, label: &str) -> String {
    let Ok(h) = synthesis_app::homes(env) else {
        return "unknown".into();
    };
    let Ok(cfg) = synthesis_app::load_config(env, &h, Some(project), ProjectTrust::Untrusted)
    else {
        return "unknown".into();
    };
    match app::window_source(&cfg, &h.synthesis, label) {
        (Some(w), src) => format!("{} ({src})", k(w)),
        (None, why) => {
            let (p, m) = label.split_once('/').unwrap_or(("provider", label));
            format!(
                "unknown ({why}); compaction assumes 200k. Set it: /context <tokens> (e.g. /context 1m) writes limit.context for \"{m}\" under providers.{p}.models in synt.json"
            )
        }
    }
}

/// Config models first, then cached (discovered) ones; reads files only, never the network.
pub fn rows(env: &Env, project: &Path) -> Vec<ModelRow> {
    let Ok(h) = synthesis_app::homes(env) else {
        return vec![];
    };
    let Ok(cfg) = synthesis_app::load_config(env, &h, Some(project), ProjectTrust::Untrusted)
    else {
        return vec![];
    };
    let mut listed: Vec<(String, String, String)> = list_models(&cfg, None)
        .into_iter()
        .map(|m| (m.value, m.provider, m.model))
        .collect();
    // one cache read per provider: a gateway lists hundreds of models, and reading the file per
    // model made this quadratic (seconds on every session switch)
    let caches: std::collections::HashMap<_, _> = cfg
        .providers
        .iter()
        .map(|(id, p)| (id.clone(), app::cache_of(&h.synthesis, id, p)))
        .collect();
    let known: std::collections::HashSet<_> = listed.iter().map(|l| l.0.clone()).collect();
    for id in cfg.providers.keys() {
        for m in caches[id].iter().flat_map(|c| c.models.keys()) {
            let value = format!("{id}/{m}");
            if !known.contains(&value) {
                listed.push((value, id.clone(), m.clone()));
            }
        }
    }
    let mut out = vec![];
    for (value, pid, model) in listed {
        let facts = caches
            .get(&pid)
            .and_then(|c| c.as_ref())
            .and_then(|c| c.models.get(&model));
        let price = app::price_of(&h.synthesis, &value, &model);
        out.push(ModelRow {
            detail: detail(facts, price),
            value: san(&value),
        });
    }
    out
}

/// `synt models --refresh` for every provider (blocking: run it off the UI thread). Always returns
/// the rows as the cache now has them, and one line per provider that failed.
pub fn refresh(env: &Env, project: &Path) -> (Vec<ModelRow>, Vec<String>) {
    let results = synthesis_app::homes(env)
        .and_then(|h| {
            synthesis_app::load_config(env, &h, Some(project), ProjectTrust::Untrusted)
                .map(|c| app::refresh_all(env, &c, &h.synthesis))
        })
        .unwrap_or_else(|e| vec![("config".into(), Err(e))]);
    (rows(env, project), errors(results))
}

/// Longest refresh error row, in characters.
const MAX_ERROR_ROW: usize = 300;

fn errors(results: Vec<(String, Result<(), String>)>) -> Vec<String> {
    results
        .into_iter()
        .filter_map(|(id, r)| {
            let line = format!("{}: {}", san(&id), san(&r.err()?));
            // one row must stay a row: a provider can send any length
            Some(match line.char_indices().nth(MAX_ERROR_ROW) {
                Some((i, _)) => format!("{}...", &line[..i]),
                None => line,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_values_are_left_out_and_known_ones_show_their_number() {
        assert_eq!(detail(None, None), "");
        let only_ctx = ModelFacts {
            context_window: Some(32_000),
            ..Default::default()
        };
        assert_eq!(detail(Some(&only_ctx), None), "ctx 32k");
        let f = ModelFacts {
            context_window: Some(200_000),
            max_output: Some(8192),
            vision: Some(false),
            effort_tiers: vec!["low".into(), "high".into()],
            ..Default::default()
        };
        assert_eq!(
            detail(Some(&f), Some((3.0, 15.0))),
            "ctx 200k \u{b7} out 8k \u{b7} vision no \u{b7} effort low|high \u{b7} $3/$15"
        );
        assert_eq!(k(1_000_000), "1m");
    }

    /// A gateway lists hundreds of models; `rows` runs on every session switch, so it must read the
    /// cache once per provider (it read it once per model: seconds, quadratic).
    #[test]
    fn rows_of_a_gateway_with_hundreds_of_models_is_fast() {
        use synthesis_providers::modelinfo as mi;
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("synt.json"),
            r#"{"$schema":"config.json/v1","model":"gw/m0","providers":{"gw":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":"http://localhost:1/v1","apiKey":"k"},"models":{"m0":{"name":"M0"}}}}}"#,
        )
        .unwrap();
        let facts = ModelFacts {
            context_window: Some(200_000),
            raw_capabilities: serde_json::json!({"note": "x".repeat(1000)})
                .as_object()
                .cloned(),
            ..Default::default()
        };
        let cache = mi::CacheFile {
            schema_version: mi::SCHEMA_VERSION,
            base_url: "http://localhost:1/v1".into(),
            protocol: "openai".into(),
            fetched_at: 1,
            models: (0..600)
                .map(|i| (format!("model-{i}"), facts.clone()))
                .collect(),
        };
        mi::write_cache(&mi::cache_path(&home, "gw"), &cache).unwrap();
        let env = Env::from_pairs([
            ("HOME", t.path().to_str().unwrap()),
            ("SYNT_HOME", home.to_str().unwrap()),
        ]);
        let t0 = std::time::Instant::now();
        let r = rows(&env, t.path());
        let d = t0.elapsed();
        assert!(r.len() >= 600, "{} rows", r.len());
        assert!(d < std::time::Duration::from_millis(500), "rows took {d:?}");
    }

    #[test]
    fn an_error_row_is_short_and_carries_no_key_from_the_provider_body() {
        let key = "sk-test-SECRETSECRETSECRET1234";
        let redaction = synthesis_providers::Redaction::new([key.to_string()]);
        let body = format!(
            r#"{{"error":{{"message":"bad key {key} {}"}}}}"#,
            "x".repeat(5000)
        );
        let e = redaction.api_error(Some(401), &body, None);
        let rows = errors(vec![("gw".into(), Err(e.message().to_string()))]);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].contains(key), "{}", rows[0]);
        assert!(
            rows[0].chars().count() <= MAX_ERROR_ROW + 3,
            "{}",
            rows[0].len()
        );
        assert!(rows[0].starts_with("gw: bad key "));
    }

    #[test]
    fn a_failed_provider_is_one_error_line_and_a_good_one_is_silent() {
        let e = errors(vec![
            ("a".into(), Ok(())),
            ("b".into(), Err("no answer in time".into())),
        ]);
        assert_eq!(e, vec!["b: no answer in time".to_string()]);
    }
}
