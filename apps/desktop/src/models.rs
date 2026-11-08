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

/// `ctx 200k · out 64k · vision yes · effort low|high · $3/$15` with `?` where nothing is known.
pub fn detail(f: Option<&ModelFacts>, price: Option<(f64, f64)>) -> String {
    let q = || "?".to_string();
    let ctx = f.and_then(|f| f.context_window).map_or_else(q, k);
    let out = f.and_then(|f| f.max_output).map_or_else(q, k);
    let vision = match f.and_then(|f| f.vision) {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => q(),
    };
    let tiers = match f.map(|f| &f.effort_tiers) {
        Some(t) if !t.is_empty() => t.join("|"),
        _ => q(),
    };
    let price = price.map_or_else(q, |(i, o)| format!("${i}/${o}"));
    format!(
        "ctx {ctx} \u{b7} out {out} \u{b7} vision {vision} \u{b7} effort {tiers} \u{b7} {price}"
    )
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
    for (id, p) in &cfg.providers {
        let cache = app::cache_of(&h.synthesis, id, p);
        for m in cache.iter().flat_map(|c| c.models.keys()) {
            let value = format!("{id}/{m}");
            if !listed.iter().any(|l| l.0 == value) {
                listed.push((value, id.clone(), m.clone()));
            }
        }
    }
    let mut out = vec![];
    for (value, pid, model) in listed {
        let facts = cfg
            .providers
            .get(&pid)
            .and_then(|p| app::cache_of(&h.synthesis, &pid, p))
            .and_then(|c| c.models.get(&model).cloned());
        let price = app::price_of(&h.synthesis, &value, &model);
        out.push(ModelRow {
            detail: detail(facts.as_ref(), price),
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
    fn unknown_values_show_a_question_mark_and_known_ones_their_number() {
        assert_eq!(
            detail(None, None),
            "ctx ? \u{b7} out ? \u{b7} vision ? \u{b7} effort ? \u{b7} ?"
        );
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
