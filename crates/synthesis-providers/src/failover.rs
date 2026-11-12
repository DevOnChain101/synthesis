//! Provider failover: when the primary gives up BEFORE the first byte (after its own retries) with
//! a rate limit, overload or server error, the same request goes to the next entry. A stream that
//! already produced an event is never switched or replayed. The switch is sticky: later requests
//! start at the entry that answered.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream;
use tokio_util::sync::CancellationToken;

use crate::capabilities::ProviderCapabilities;
use crate::error::{ErrorKind, ProviderError};
use crate::http::EventStream;
use crate::provider::{ModelInfo, ModelProvider};
use crate::types::Request;
use synthesis_config::Protocol;

/// One fallback: its provider, the model it is asked for and its `provider/model` label.
pub struct Fallback {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub label: String,
}

/// Called with `(from label, to label, reason)` on each switch (maps to `model.failover`).
pub type OnFailover = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

pub struct FailoverProvider {
    primary: Arc<dyn ModelProvider>,
    primary_label: String,
    fallbacks: Arc<Vec<Fallback>>,
    /// 0 = primary, n = `fallbacks[n - 1]`.
    active: Arc<AtomicUsize>,
    on_failover: OnFailover,
}

impl FailoverProvider {
    pub fn new(
        primary: Arc<dyn ModelProvider>,
        primary_label: String,
        fallbacks: Vec<Fallback>,
        on_failover: OnFailover,
    ) -> Self {
        Self {
            primary,
            primary_label,
            fallbacks: Arc::new(fallbacks),
            active: Arc::new(AtomicUsize::new(0)),
            on_failover,
        }
    }
}

fn failoverable(e: &ProviderError) -> bool {
    !e.mid_stream
        && matches!(
            e.kind,
            ErrorKind::RateLimit | ErrorKind::Overloaded | ErrorKind::Server
        )
}

impl ModelProvider for FailoverProvider {
    fn id(&self) -> &str {
        self.primary.id()
    }
    fn protocol(&self) -> Protocol {
        self.primary.protocol()
    }
    /// The primary's: a fallback may have other limits (documented limitation).
    fn capabilities(&self, model: &str) -> ProviderCapabilities {
        self.primary.capabilities(model)
    }

    fn stream(&self, req: &Request, cancel: CancellationToken) -> EventStream {
        let (primary, label, fbs, active, notify) = (
            self.primary.clone(),
            self.primary_label.clone(),
            self.fallbacks.clone(),
            self.active.clone(),
            self.on_failover.clone(),
        );
        let req = req.clone();
        let run = async move {
            let mut at = active.load(Ordering::SeqCst);
            let start = |at: usize| {
                if at == 0 {
                    primary.stream(&req, cancel.clone())
                } else {
                    let f = &fbs[at - 1];
                    let mut r = req.clone();
                    r.model.clone_from(&f.model);
                    f.provider.stream(&r, cancel.clone())
                }
            };
            let mut cur = start(at);
            let mut first = cur.next().await;
            while let Some(Err(e)) = &first
                && failoverable(e)
                && at < fbs.len()
            {
                let from = if at == 0 { &label } else { &fbs[at - 1].label };
                notify(from, &fbs[at].label, e.kind.as_str());
                at += 1;
                active.store(at, Ordering::SeqCst);
                cur = start(at);
                first = cur.next().await;
            }
            stream::iter(first).chain(cur)
        };
        stream::once(run).flatten().boxed()
    }

    fn list_models<'a>(
        &'a self,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>, ProviderError>> {
        self.primary.list_models(cancel)
    }

    fn refresh_model_info<'a>(
        &'a self,
        force: bool,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        self.primary.refresh_model_info(force, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Content, Message, Role, StreamEvent};
    use std::sync::Mutex;

    /// Plays `items` for every request and records the model asked for.
    struct Mock {
        items: Vec<Result<StreamEvent, ProviderError>>,
        models: Mutex<Vec<String>>,
    }

    impl ModelProvider for Mock {
        fn id(&self) -> &str {
            "mock"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Anthropic
        }
        fn capabilities(&self, _: &str) -> ProviderCapabilities {
            ProviderCapabilities::defaults(Protocol::Anthropic)
        }
        fn stream(&self, req: &Request, _: CancellationToken) -> EventStream {
            self.models.lock().unwrap().push(req.model.clone());
            stream::iter(self.items.clone()).boxed()
        }
        fn list_models<'a>(
            &'a self,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<Vec<ModelInfo>, ProviderError>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    fn mock(items: Vec<Result<StreamEvent, ProviderError>>) -> Arc<Mock> {
        Arc::new(Mock {
            items,
            models: Mutex::default(),
        })
    }

    fn req() -> Request {
        Request {
            model: "m1".into(),
            max_tokens: 10,
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: Content::Text("hi".into()),
            }],
            tools: vec![],
            temperature: None,
            thinking: None,
            reasoning_effort: None,
        }
    }

    fn setup(primary: Arc<Mock>, fb: Arc<Mock>) -> (FailoverProvider, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let s = seen.clone();
        let p = FailoverProvider::new(
            primary,
            "a/m1".into(),
            vec![Fallback {
                provider: fb,
                model: "m2".into(),
                label: "b/m2".into(),
            }],
            Arc::new(move |f, t, r| s.lock().unwrap().push(format!("{f}->{t}:{r}"))),
        );
        (p, seen)
    }

    async fn drain(p: &FailoverProvider) -> Vec<Result<StreamEvent, ProviderError>> {
        p.stream(&req(), CancellationToken::new()).collect().await
    }

    #[tokio::test]
    async fn pre_byte_overload_moves_to_the_fallback_and_stays_there() {
        let primary = mock(vec![Err(ProviderError::new(ErrorKind::Overloaded, "busy"))]);
        let fb = mock(vec![Ok(StreamEvent::Ping)]);
        let (p, seen) = setup(primary.clone(), fb.clone());
        assert!(matches!(drain(&p).await[..], [Ok(StreamEvent::Ping)]));
        assert_eq!(*seen.lock().unwrap(), ["a/m1->b/m2:overloaded"]);
        assert_eq!(*fb.models.lock().unwrap(), ["m2"]);
        // sticky: the next request does not touch the primary again
        drain(&p).await;
        assert_eq!(primary.models.lock().unwrap().len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn never_switches_after_the_first_event_or_for_other_errors() {
        let mut mid = ProviderError::new(ErrorKind::Server, "cut");
        mid.mid_stream = true;
        let primary = mock(vec![Ok(StreamEvent::Ping), Err(mid)]);
        let fb = mock(vec![Ok(StreamEvent::Ping)]);
        let (p, seen) = setup(primary, fb.clone());
        let got = drain(&p).await;
        assert!(matches!(got[..], [Ok(_), Err(_)]));
        assert!(fb.models.lock().unwrap().is_empty() && seen.lock().unwrap().is_empty());

        let primary = mock(vec![Err(ProviderError::new(ErrorKind::Auth, "bad key"))]);
        let (p, seen) = setup(primary, fb.clone());
        assert!(matches!(drain(&p).await[..], [Err(_)]));
        assert!(fb.models.lock().unwrap().is_empty() && seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_last_error_is_returned_when_every_entry_fails() {
        let e = || Err(ProviderError::new(ErrorKind::RateLimit, "429"));
        let (p, seen) = setup(mock(vec![e()]), mock(vec![e()]));
        assert!(matches!(drain(&p).await[..], [Err(_)]));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
