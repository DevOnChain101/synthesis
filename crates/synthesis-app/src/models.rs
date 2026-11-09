//! `synt models --refresh` as a library: ask a provider for its model list (bounded, into the
//! cache) and read the cached facts back. Shared by `synt models` and the desktop model picker.

use std::path::Path;
use std::time::Duration;

use synthesis_config::{Config, Env, Protocol, Provider, resolve_credential};
use synthesis_providers::{
    ErrorKind, ModelProvider, NetSettings, ProviderError, ProviderSpec, RetryPolicy, modelinfo,
};
use tokio_util::sync::CancellationToken;

/// Longest a refresh waits for one provider.
pub const REFRESH_DEADLINE: Duration = Duration::from_secs(30);

/// Forces a model-list fetch; a provider that does not answer within `deadline` is an error.
pub async fn refresh_bounded(prov: &dyn ModelProvider, deadline: Duration) -> Result<(), String> {
    tokio::time::timeout(
        deadline,
        prov.refresh_model_info(true, &CancellationToken::new()),
    )
    .await
    .unwrap_or_else(|_| Err(ProviderError::new(ErrorKind::Network, "no answer in time")))
    .map_err(|e| e.message().to_string())
}

/// Connects the configured provider `id` and refreshes its cache. Blocking: own runtime, so call
/// it from a worker thread, never from inside a tokio runtime.
pub fn refresh_provider(
    env: &Env,
    synthesis_home: &Path,
    id: &str,
    p: &Provider,
) -> Result<(), String> {
    let cred = resolve_credential(id, Some(p), synthesis_home, env);
    let prov = ProviderSpec::from_config(id, p, cred.as_ref())
        .and_then(|s| {
            s.connect_cached(
                env,
                &NetSettings::default(),
                RetryPolicy::default(),
                Some(modelinfo::cache_path(synthesis_home, id)),
            )
        })
        .map_err(|e| e.message().to_string())?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(refresh_bounded(&*prov, REFRESH_DEADLINE))
}

/// Refreshes every provider that is not `discover: false`; one result per provider.
pub fn refresh_all(
    env: &Env,
    cfg: &Config,
    synthesis_home: &Path,
) -> Vec<(String, Result<(), String>)> {
    cfg.providers
        .iter()
        .filter(|(_, p)| p.discover != Some(false))
        .map(|(id, p)| (id.clone(), refresh_provider(env, synthesis_home, id, p)))
        .collect()
}

/// The cached model facts of one provider (`None`: no usable cache).
pub fn cache_of(synthesis_home: &Path, id: &str, p: &Provider) -> Option<modelinfo::CacheFile> {
    let base = match p.protocol {
        Protocol::Anthropic => synthesis_providers::anthropic::base_url(p.base_url.as_deref()),
        Protocol::OpenAi => synthesis_providers::openai::base_url(p.base_url.as_deref()),
    };
    modelinfo::read_cache(
        &modelinfo::cache_path(synthesis_home, id),
        base.trim_end_matches('/'),
        p.protocol,
    )
}

/// `(input, output)` USD per million tokens from `synt.pricing` of the user settings, if set.
pub fn price_of(synthesis_home: &Path, label: &str, model: &str) -> Option<(f64, f64)> {
    crate::run::pricing(&synthesis_home.join("settings.json"), label, model)
        .map(|p| (p.input_per_mtok, p.output_per_mtok))
}

/// Where the context window of `provider/model` comes from, in the order of docs/specs/config.md
/// 6.2: `limit.context` of the model in synt.json, the model-list cache, the built-in table
/// (api.openai.com only). Nothing is guessed from the model's name: no source = unknown.
pub fn window_source(cfg: &Config, synthesis_home: &Path, label: &str) -> (Option<u64>, String) {
    let Some((p, m)) = label.split_once('/') else {
        return (None, "no provider in the model id".into());
    };
    let Some(prov) = cfg.providers.get(p) else {
        return (None, format!("provider \"{p}\" is not configured"));
    };
    let declared = prov
        .models_meta
        .as_ref()
        .and_then(|t| t.get(m))
        .and_then(|t| t.context_window)
        .map(|w| w as u64);
    if let Some(w) = declared {
        return (Some(w), "limit.context in synt.json".into());
    }
    let cache = cache_of(synthesis_home, p, prov);
    if let Some(w) = cache
        .as_ref()
        .and_then(|c| c.models.get(m))
        .and_then(|f| f.context_window)
    {
        return (Some(w), "the provider's model list (cache)".into());
    }
    let native = prov.protocol == Protocol::OpenAi
        && synthesis_providers::openai::base_url(prov.base_url.as_deref())
            .starts_with("https://api.openai.com");
    if let Some((w, _)) = modelinfo::builtin(native, m) {
        return (Some(w), "built-in table of documented limits".into());
    }
    let why = match cache {
        None => "no model-list cache for this provider".to_string(),
        Some(c) if c.models.contains_key(m) => "the provider's list gives no window for it".into(),
        Some(_) => "the provider's list does not contain it".into(),
    };
    (None, why)
}

/// `200000`, `200k`, `1m`, `1.5m`, `200,000` -> tokens, 1k..=100M (the range a provider answer is
/// also held to).
pub fn parse_tokens(input: &str) -> Result<u64, String> {
    let t: String = input
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ',' | '_'))
        .collect();
    let (num, mult) = match t.strip_suffix('k') {
        Some(n) => (n, 1e3),
        None => match t.strip_suffix('m') {
            Some(n) => (n, 1e6),
            None => (t.as_str(), 1.0),
        },
    };
    let bad = || {
        let shown: String = synthesis_harness::sanitize_for_display(input.trim())
            .chars()
            .take(40)
            .collect();
        format!("\"{shown}\" is not a token count (e.g. 200000, 200k, 1m)")
    };
    let v = num.parse::<f64>().map_err(|_| bad())? * mult;
    if !v.is_finite() || v.fract().abs() > 1e-6 {
        return Err(bad());
    }
    let v = v.round() as u64;
    if !(1_000..=100_000_000).contains(&v) {
        return Err(format!("{v} tokens is outside 1k..100M"));
    }
    Ok(v)
}

/// Sets (`Some`) or removes (`None`) `providers.<provider>.models.<model>.limit.context` in the
/// USER synt.json and nothing else: other keys and their order stay, credential fields are neither
/// read nor shown, a file that does not parse or a symlink is refused, a project or managed file
/// is never opened. The provider must already be in the file with an object-form `models`.
pub fn set_context_limit(
    synthesis_home: &Path,
    provider: &str,
    model: &str,
    tokens: Option<u64>,
) -> Result<(), String> {
    use serde_json::{Map, Value};
    let path = synthesis_home.join("synt.json");
    // the file and the home folder it sits in: neither may be a link
    for p in [path.as_path(), synthesis_home] {
        if std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(format!(
                "{} is a symbolic link; edit the real file by hand",
                p.display()
            ));
        }
    }
    // nothing is created for a file that is not there (so no folder either)
    if !path.is_file() {
        return Err(format!(
            "{} does not exist: nothing to edit",
            path.display()
        ));
    }
    let no = |why: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, why);
    synthesis_settings::edit_json_atomic(&path, "synt.json", |root| {
        let prov = root
            .get_mut("providers")
            .and_then(|p| p.get_mut(provider))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| no(format!("provider \"{provider}\" is not in your synt.json")))?;
        let models = match prov.get_mut("models") {
            Some(Value::Object(m)) => m,
            _ => {
                return Err(no(format!(
                    "providers.{provider}.models in synt.json is not an object of models: declare the model there first"
                )));
            }
        };
        match tokens {
            Some(n) => {
                // `models` is also the list of models a provider offers: the first entry of an
                // empty one would make every other (discovered) model "not declared"
                if models.is_empty() {
                    return Err(no(format!(
                        "declare the model first in synt.json (providers.{provider}.models) so it does not restrict the other models"
                    )));
                }
                let entry = models
                    .entry(model.to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                let entry = entry
                    .as_object_mut()
                    .ok_or_else(|| no(format!("the entry of \"{model}\" is not an object")))?;
                let limit = entry
                    .entry("limit")
                    .or_insert_with(|| Value::Object(Map::new()));
                limit
                    .as_object_mut()
                    .ok_or_else(|| no("\"limit\" is not an object".into()))?
                    .insert("context".into(), Value::from(n));
            }
            None => {
                if let Some(entry) = models.get_mut(model).and_then(Value::as_object_mut)
                    && let Some(limit) = entry.get_mut("limit").and_then(Value::as_object_mut)
                {
                    limit.remove("context");
                    if limit.is_empty() {
                        entry.remove("limit");
                    }
                }
            }
        }
        Ok(())
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;

    /// A provider whose refresh answers what it is told, or never.
    struct Fake(Option<Result<(), ProviderError>>);
    impl ModelProvider for Fake {
        fn id(&self) -> &str {
            "fake"
        }
        fn protocol(&self) -> Protocol {
            Protocol::OpenAi
        }
        fn capabilities(&self, _: &str) -> synthesis_providers::ProviderCapabilities {
            unimplemented!()
        }
        fn stream(
            &self,
            _: &synthesis_providers::Request,
            _: CancellationToken,
        ) -> synthesis_providers::EventStream {
            unimplemented!()
        }
        fn list_models<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<synthesis_providers::ModelInfo>, ProviderError>> {
            unimplemented!()
        }
        fn refresh_model_info<'a>(
            &'a self,
            _: bool,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<(), ProviderError>> {
            let r = self.0.clone();
            Box::pin(async move {
                match r {
                    Some(r) => r,
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn run(f: Fake, d: Duration) -> Result<(), String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(refresh_bounded(&f, d))
    }

    fn cfg_with(declared: Option<u64>) -> Config {
        let mut c = Config::default();
        let mut p = Provider {
            base_url: Some("http://localhost:8688/v1".into()),
            protocol: Protocol::OpenAi,
            models_meta: Some(Default::default()),
            ..Default::default()
        };
        p.models_meta.as_mut().unwrap().insert(
            "antigravity/gemini-3.8-flash-low".to_string(),
            synthesis_config::ModelMeta {
                context_window: declared.map(|w| w as f64),
                ..Default::default()
            },
        );
        c.providers.insert("omniroute".into(), p);
        c
    }

    fn write_list(home: &Path, c: &Config, ids: &[(&str, Option<u64>)]) {
        let p = &c.providers["omniroute"];
        let cache = modelinfo::CacheFile {
            schema_version: modelinfo::SCHEMA_VERSION,
            base_url: "http://localhost:8688/v1".into(),
            protocol: modelinfo::protocol_name(p.protocol).into(),
            fetched_at: modelinfo::unix_now(),
            models: ids
                .iter()
                .map(|(i, w)| {
                    (
                        i.to_string(),
                        modelinfo::ModelFacts {
                            context_window: *w,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        };
        modelinfo::write_cache(&modelinfo::cache_path(home, "omniroute"), &cache).unwrap();
    }

    #[test]
    fn the_window_of_a_slash_id_comes_from_config_then_cache_and_is_never_guessed() {
        let home = tempfile::tempdir().unwrap();
        let label = "omniroute/antigravity/gemini-3.8-flash-low";
        // a gateway list without the declared id (as the owner's omniroute): unknown, with the reason
        let c = cfg_with(None);
        write_list(home.path(), &c, &[("cx/gpt-5.6-terra", Some(872_000))]);
        let (w, why) = window_source(&c, home.path(), label);
        assert_eq!(w, None);
        assert_eq!(why, "the provider's list does not contain it");
        // another model of the same gateway has its window
        assert_eq!(
            window_source(&c, home.path(), "omniroute/cx/gpt-5.6-terra").0,
            Some(872_000)
        );
        // the id listed with a window
        write_list(
            home.path(),
            &c,
            &[("antigravity/gemini-3.8-flash-low", Some(1_000_000))],
        );
        assert_eq!(
            window_source(&c, home.path(), label),
            (Some(1_000_000), "the provider's model list (cache)".into())
        );
        // listed without a window: unknown, and it says so
        write_list(
            home.path(),
            &c,
            &[("antigravity/gemini-3.8-flash-low", None)],
        );
        assert_eq!(
            window_source(&c, home.path(), label).1,
            "the provider's list gives no window for it"
        );
        // limit.context in synt.json wins over the cache
        write_list(
            home.path(),
            &c,
            &[("antigravity/gemini-3.8-flash-low", Some(1_000_000))],
        );
        assert_eq!(
            window_source(&cfg_with(Some(500_000)), home.path(), label),
            (Some(500_000), "limit.context in synt.json".into())
        );
        // the provider prefix is part of the label, not of the cache key
        assert_eq!(
            window_source(&c, home.path(), "antigravity/gemini-3.8-flash-low").0,
            None
        );
    }

    fn home_with(json: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("synt.json"), json).unwrap();
        d
    }

    const FILE: &str = r#"{"$schema":"config.json/v1","model":"omniroute/a","providers":{"omniroute":{"options":{"baseURL":"http://x/v1","apiKey":"{env:OMNI_KEY}"},"discover":true,"models":{"a":{"name":"A","capabilities":{"toolCalling":true}},"b":{}}},"other":{}}}"#;

    #[test]
    fn token_counts_parse_with_suffixes_and_are_range_checked() {
        for (i, n) in [
            ("200000", 200_000),
            ("200k", 200_000),
            ("1m", 1_000_000),
            ("1.5M", 1_500_000),
            ("200,000", 200_000),
            (" 64K ", 64_000),
        ] {
            assert_eq!(parse_tokens(i), Ok(n), "{i}");
        }
        for bad in [
            "", "abc", "999", "0", "-5k", "101m", "1.2345k", "nan", "1e9",
        ] {
            assert!(parse_tokens(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_limit_is_set_and_removed_and_nothing_else_changes() {
        let d = home_with(FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                d.path().join("synt.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(d.path().join("synt.json")).unwrap())
                .unwrap()
        };
        set_context_limit(d.path(), "omniroute", "a", Some(1_000_000)).unwrap();
        let v = read();
        assert_eq!(
            v["providers"]["omniroute"]["models"]["a"]["limit"]["context"],
            1_000_000
        );
        // extra keys, the credential reference and the order are kept
        let mut want: serde_json::Value = serde_json::from_str(FILE).unwrap();
        want["providers"]["omniroute"]["models"]["a"]["limit"] =
            serde_json::json!({"context": 1_000_000});
        assert_eq!(v, want);
        assert_eq!(
            v["providers"]["omniroute"]["options"]["apiKey"],
            "{env:OMNI_KEY}"
        );
        assert_eq!(v.as_object().unwrap().keys().next().unwrap(), "$schema");
        // another model gets its own entry; `limit` of the first stays
        set_context_limit(d.path(), "omniroute", "b", Some(64_000)).unwrap();
        assert_eq!(
            read()["providers"]["omniroute"]["models"]["b"]["limit"]["context"],
            64_000
        );
        // unset removes the number and the empty `limit`, not the rest of the entry
        set_context_limit(d.path(), "omniroute", "a", None).unwrap();
        let v = read();
        assert!(
            v["providers"]["omniroute"]["models"]["a"]
                .get("limit")
                .is_none()
        );
        assert_eq!(v["providers"]["omniroute"]["models"]["a"]["name"], "A");
        // unsetting what is not there is fine
        set_context_limit(d.path(), "omniroute", "zzz", None).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(d.path().join("synt.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o077, 0, "not wider than 0600 for a file that was 0600");
        }
    }

    #[test]
    fn what_cannot_be_edited_safely_is_refused_and_left_alone() {
        // a provider or a models list in another shape
        let d = home_with(FILE);
        assert!(
            set_context_limit(d.path(), "nope", "a", Some(5000))
                .unwrap_err()
                .contains("not in your synt.json")
        );
        assert!(
            set_context_limit(d.path(), "other", "a", Some(5000))
                .unwrap_err()
                .contains("declare the model")
        );
        let d2 = home_with(r#"{"providers":{"p":{"models":["a","b"]}}}"#);
        assert!(set_context_limit(d2.path(), "p", "a", Some(5000)).is_err());
        // an empty `models` is an open list: the first entry would restrict it, so it is refused
        let d5 = home_with(r#"{"providers":{"p":{"discover":true,"models":{}}}}"#);
        let e = set_context_limit(d5.path(), "p", "a", Some(5000)).unwrap_err();
        assert!(e.contains("declare the model first"), "{e}");
        assert_eq!(
            std::fs::read_to_string(d5.path().join("synt.json")).unwrap(),
            r#"{"providers":{"p":{"discover":true,"models":{}}}}"#
        );
        // unparseable: refused, bytes untouched
        let d3 = home_with("{ not json");
        assert!(set_context_limit(d3.path(), "p", "a", Some(5000)).is_err());
        assert_eq!(
            std::fs::read_to_string(d3.path().join("synt.json")).unwrap(),
            "{ not json"
        );
        // a missing file is not created
        let d4 = tempfile::tempdir().unwrap();
        assert!(set_context_limit(d4.path(), "p", "a", Some(5000)).is_err());
        // a project file next to it is never written (only <home>/synt.json is opened)
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".synt")).unwrap();
        let pf = proj.path().join(".synt/config.json");
        std::fs::write(&pf, "{}").unwrap();
        set_context_limit(d.path(), "omniroute", "a", Some(5000)).unwrap();
        assert_eq!(std::fs::read_to_string(&pf).unwrap(), "{}");
    }

    #[test]
    fn a_typed_value_is_echoed_short_and_clean() {
        let e = parse_tokens(&format!("x\u{1b}[31m{}", "y".repeat(500))).unwrap_err();
        assert!(!e.contains('\u{1b}') && e.len() < 120, "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_home_is_refused_and_a_missing_file_creates_nothing() {
        let real = home_with(FILE);
        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("home");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        assert!(
            set_context_limit(&link, "omniroute", "a", Some(5000))
                .unwrap_err()
                .contains("symbolic link")
        );
        // a folder that does not exist is not created by a refused edit
        let missing = holder.path().join("nope").join("home");
        assert!(set_context_limit(&missing, "p", "a", Some(5000)).is_err());
        assert!(!holder.path().join("nope").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_synt_json_is_refused() {
        let d = home_with(FILE);
        let real = d.path().join("real.json");
        std::fs::rename(d.path().join("synt.json"), &real).unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("synt.json")).unwrap();
        assert!(
            set_context_limit(d.path(), "omniroute", "a", Some(5000))
                .unwrap_err()
                .contains("symbolic link")
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), FILE);
    }

    #[test]
    fn refresh_reports_ok_provider_error_and_timeout() {
        assert_eq!(run(Fake(Some(Ok(()))), REFRESH_DEADLINE), Ok(()));
        let e = ProviderError::new(ErrorKind::Network, "boom");
        assert_eq!(
            run(Fake(Some(Err(e))), REFRESH_DEADLINE),
            Err("boom".into())
        );
        assert_eq!(
            run(Fake(None), Duration::from_millis(50)),
            Err("no answer in time".into())
        );
    }
}
