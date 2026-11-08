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
