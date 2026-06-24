//! Transient-error retry wrapper around a [`CompletionModel`].
//!
//! [`RetryingModel`] wraps the OpenAi-backed completion model so that each
//! individual model call retries on transient HTTP failures (request timeouts,
//! connection errors, 408/425/429/5xx) with bounded exponential backoff.
//!
//! Retrying at the model-call layer -- rather than around `agent.prompt(...)`
//! -- is deliberate: a single user turn drives a model -> tool -> model loop,
//! and wrapping the whole prompt would re-execute already-run container tool
//! calls when a *later* model call fails. A failed model call has produced no
//! tool calls yet, so retrying just that call replays nothing observable.

use std::time::Duration;

use rand::RngExt;
use rig::completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse};
use rig::streaming::StreamingCompletionResponse;

/// Retry attempts after the initial call, so up to `MAX_RETRIES + 1` calls.
const MAX_RETRIES: usize = 2;
/// First backoff delay; doubles each retry, capped at [`MAX_DELAY`].
const BASE_DELAY: Duration = Duration::from_secs(1);
/// Ceiling on a single backoff delay, before jitter is applied.
const MAX_DELAY: Duration = Duration::from_secs(30);

/// Wraps a [`CompletionModel`], retrying transient failures on every call.
#[derive(Clone)]
pub struct RetryingModel<M> {
    inner: M,
}

impl<M> RetryingModel<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<M: CompletionModel> CompletionModel for RetryingModel<M> {
    type Response = M::Response;
    type StreamingResponse = M::StreamingResponse;
    type Client = M::Client;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::new(M::make(client, model))
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<CompletionResponse<Self::Response>, CompletionError> {
        let mut attempt = 0;
        loop {
            match self.inner.completion(request.clone()).await {
                Ok(response) => return Ok(response),
                Err(err) if attempt < MAX_RETRIES && is_transient(&err) => {
                    let delay = backoff(attempt);
                    eprintln!(
                        "[outrig] LLM call failed ({err}); retrying in {:.1}s \
                         (attempt {}/{MAX_RETRIES})",
                        delay.as_secs_f64(),
                        attempt + 1,
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> std::result::Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError>
    {
        // The OpenAi path is non-streaming in outrig; delegate without retry so
        // the wrapper stays a faithful `CompletionModel` for any future caller.
        self.inner.stream(request).await
    }
}

/// Classify an error as a transient failure worth retrying: request timeouts,
/// connection errors, and the retry-friendly HTTP status codes. Everything else
/// (other 4xx, malformed/JSON responses, provider-reported errors) is terminal.
fn is_transient(err: &CompletionError) -> bool {
    use rig::http_client::Error as HttpError;
    match err {
        CompletionError::HttpError(HttpError::InvalidStatusCode(code))
        | CompletionError::HttpError(HttpError::InvalidStatusCodeWithMessage(code, _)) => {
            matches!(code.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
        }
        // `Instance` wraps the underlying reqwest transport error -- connection
        // failures, read timeouts, and the like -- all retry-worthy.
        CompletionError::HttpError(HttpError::Instance(_)) => true,
        _ => false,
    }
}

/// Pre-jitter backoff in seconds: `BASE_DELAY * 2^attempt`, capped at
/// [`MAX_DELAY`]. Factored out so the (deterministic) schedule is testable.
fn backoff_secs(attempt: usize) -> f64 {
    let factor = 2f64.powi(attempt.min(16) as i32);
    (BASE_DELAY.as_secs_f64() * factor).min(MAX_DELAY.as_secs_f64())
}

/// Backoff delay for a given retry attempt, with equal jitter: the capped delay
/// scaled by a random factor in `[0.5, 1.0]` to avoid synchronized retries.
fn backoff(attempt: usize) -> Duration {
    let frac = rand::rng().random_range(0.5..=1.0);
    Duration::from_secs_f64(backoff_secs(attempt) * frac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use rig::http_client::Error as HttpError;

    fn http_status(code: u16) -> CompletionError {
        CompletionError::HttpError(HttpError::InvalidStatusCodeWithMessage(
            StatusCode::from_u16(code).unwrap(),
            "boom".to_string(),
        ))
    }

    #[test]
    fn retryable_status_codes_are_transient() {
        for code in [408, 425, 429, 500, 502, 503, 504] {
            assert!(is_transient(&http_status(code)), "{code} should retry");
        }
    }

    #[test]
    fn client_errors_are_terminal() {
        for code in [400, 401, 403, 404, 422] {
            assert!(!is_transient(&http_status(code)), "{code} should not retry");
        }
    }

    #[test]
    fn transport_errors_are_transient() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        let err = CompletionError::HttpError(HttpError::Instance(Box::new(io)));
        assert!(is_transient(&err));
    }

    #[test]
    fn non_http_errors_are_terminal() {
        assert!(!is_transient(&CompletionError::ProviderError(
            "nope".into()
        )));
        assert!(!is_transient(&CompletionError::ResponseError(
            "nope".into()
        )));
    }

    #[test]
    fn backoff_schedule_grows_then_saturates() {
        // 1, 2, 4, 8, 16, then capped at MAX_DELAY (30).
        assert_eq!(backoff_secs(0), 1.0);
        assert_eq!(backoff_secs(1), 2.0);
        assert_eq!(backoff_secs(2), 4.0);
        let max = MAX_DELAY.as_secs_f64();
        assert_eq!(backoff_secs(5), max);
        assert_eq!(backoff_secs(50), max);
        // Monotonic non-decreasing.
        for a in 0..20 {
            assert!(backoff_secs(a) <= backoff_secs(a + 1));
        }
    }

    #[test]
    fn backoff_jitter_stays_within_bounds() {
        for attempt in 0..=5 {
            let cap = backoff_secs(attempt);
            for _ in 0..100 {
                let d = backoff(attempt).as_secs_f64();
                assert!(d >= cap * 0.5 - f64::EPSILON, "{d} < {}", cap * 0.5);
                assert!(d <= cap + f64::EPSILON, "{d} > {cap}");
            }
        }
    }
}
