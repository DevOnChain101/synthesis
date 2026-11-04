//! The single retry layer (Node stacked SDK maxRetries 4 under the agent's 3; we keep one).
//!
//! Replay rule: only the request/response-header phase is retried. Once the first response byte
//! of a stream has been handed to the consumer nothing is replayed (`ProviderError::mid_stream`),
//! because the consumer may already have shown text or started acting on tool input. (This layer
//! never replays; the harness may re-request the whole turn after a mid-stream failure, see
//! `HarnessConfig::stream_recoveries`: it runs tools only after a stream completed.) Model
//! requests have no server-side effects of their own, so retrying before streaming starts is
//! idempotent.

use crate::error::ProviderError;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub type RetryObserver = Arc<dyn Fn(&RetryNotice) + Send + Sync>;

#[derive(Clone)]
pub struct RetryPolicy {
    /// Retries after the first attempt (Node agent: 3).
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// A `Retry-After` longer than this is not waited for: the error is returned instead.
    pub max_retry_after: Duration,
    /// Called before each wait (maps to the `model.retry` event).
    pub on_retry: Option<RetryObserver>,
}

#[derive(Debug, Clone)]
pub struct RetryNotice {
    /// 1-based number of the retry about to happen.
    pub attempt: u32,
    pub max: u32,
    pub error: String,
    pub delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(60),
            on_retry: None,
        }
    }
}

impl RetryPolicy {
    /// `base * 2^(attempt-1)` with +-25% jitter (Node: 0.75..1.25), capped at `max_delay`.
    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt - 1));
        exp.mul_f64(0.75 + fastrand::f64() * 0.5)
            .min(self.max_delay)
    }

    /// `None` = give up (a server-requested wait beyond `max_retry_after`).
    fn delay(&self, attempt: u32, e: &ProviderError) -> Option<Duration> {
        match e.retry_after {
            Some(ra) if ra > self.max_retry_after => None,
            Some(ra) => Some(ra),
            None => Some(self.backoff(attempt)),
        }
    }

    pub async fn run<T, F, Fut>(
        &self,
        cancel: &CancellationToken,
        mut f: F,
    ) -> Result<T, ProviderError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ProviderError>>,
    {
        let mut attempt = 0;
        loop {
            let res = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ProviderError::cancelled()),
                r = f() => r,
            };
            let e = match res {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            attempt += 1;
            if attempt > self.max_retries || !e.retryable() {
                return Err(e);
            }
            let Some(delay) = self.delay(attempt, &e) else {
                return Err(e);
            };
            if let Some(cb) = &self.on_retry {
                cb(&RetryNotice {
                    attempt,
                    max: self.max_retries,
                    error: e.to_string(),
                    delay,
                });
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ProviderError::cancelled()),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}
