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
