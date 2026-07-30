//! Transient-error retry, at the HTTP layer.
//!
//! [`RetryingHttpClient`] is a [`rig::http_client::HttpClientExt`] backed by a
//! `reqwest::Client`. Every request it sends retries transient failures --
//! request timeouts, connection errors, and the retry-friendly HTTP statuses
//! (408/425/429/5xx) -- honoring a `Retry-After` header when the server sends
//! one and falling back to jittered exponential backoff when it does not. The
//! whole loop is bounded by a wall-clock budget rather than an attempt count,
//! because what a rate limit asks of a client is a *wait*, not a number of
//! tries.
//!
//! Retrying *here* rather than around a [`CompletionModel`] call is what makes
//! `Retry-After` reachable at all: rig's `http_client::Error` carries only a
//! status code and a body string, so by the time a failure has become a
//! `CompletionError` the headers are gone.
//!
//! It also preserves the property the model-layer wrapper was built for. A
//! single user turn drives a model -> tool -> model loop, and retrying the
//! whole prompt would re-execute already-run container tool calls when a
//! *later* model call fails. One HTTP request is below even a single model
//! call, so a retry replays exactly one request and nothing observable.
//!
//! The obvious "just use the ecosystem crate" answer is `reqwest-retry`, which
//! rig itself carries as a dev-dependency. It does not fit: as of 0.9 it does
//! not read `Retry-After` at all, which is the feature this exists for.
//!
//! [`CompletionModel`]: rig::completion::CompletionModel

use std::time::Duration;

use bytes::Bytes;
use rand::RngExt;
use reqwest::StatusCode;
use rig::completion::{CompletionError, PromptError};
use rig::http_client::{
    Error as HttpError, HeaderMap, HttpClientExt, LazyBody, Method, MultipartForm, Request,
    Response, StreamingResponse, Uri,
};
use rig::wasm_compat::WasmCompatSend;

/// First backoff delay; doubles each attempt, capped at [`RetryPolicy::max_delay`].
const BASE_DELAY: Duration = Duration::from_secs(1);
/// Ceiling on a single *backoff* delay, before jitter is applied.
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Ceiling on a server-named `Retry-After`. A misconfigured proxy can name an
/// hour; past this we stop believing it. Deliberately not [`MAX_DELAY`], which
/// caps only our own curve -- a real `Retry-After: 60` must be honored in full.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// Floor on any delay, so a `Retry-After: 0` cannot become a busy loop.
const MIN_DELAY: Duration = Duration::from_millis(100);

/// Knobs for one client's retry loop. `Copy` so the whole policy moves into a
/// `'static` per-request future without an `Arc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Wall clock from the first attempt's start, including the time each
    /// attempt spends in flight -- not only the time spent sleeping. Zero
    /// disables retries.
    pub budget: Duration,
    /// First backoff delay, doubling each attempt.
    pub base_delay: Duration,
    /// Ceiling on one backoff delay. Does not cap a server's `Retry-After`.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(outrig::config::DEFAULT_RETRY_BUDGET_SECS),
            base_delay: BASE_DELAY,
            max_delay: MAX_DELAY,
        }
    }
}

/// A `reqwest::Client` that retries transient failures, handed to rig in place
/// of a bare one. See the module doc for why the retry lives at this layer.
///
/// `Default` is never used by outrig -- `build_agent` always supplies a
/// configured client -- but rig's `CompletionModel` impls bound their HTTP
/// backend on `Default`, so the derive has to exist and has to be sane. It
/// yields the shipped policy against a default `reqwest::Client`, which
/// notably has *no* request timeout; the configured path sets one.
#[derive(Clone, Debug, Default)]
pub struct RetryingHttpClient {
    inner: reqwest::Client,
    policy: RetryPolicy,
}

impl RetryingHttpClient {
    pub fn new(inner: reqwest::Client, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }
}

impl HttpClientExt for RetryingHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = rig::http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        let client = self.inner.clone();
        let policy = self.policy;
        let (parts, body) = req.into_parts();
        // Converted once: `Bytes::clone` is a refcount bump, so replaying the
        // body on each attempt costs nothing.
        let body: Bytes = body.into();
        send_with_retry(client, policy, parts.method, parts.uri, parts.headers, body)
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = rig::http_client::Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        // No retry: a multipart form is not cloneable, so there is no body to
        // replay. outrig has no multipart path today; a future one would need
        // the form rebuilt per attempt.
        HttpClientExt::send_multipart(&self.inner, req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = rig::http_client::Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        // No retry: outrig's remote turns are non-streaming (the streaming
        // path is `local-llm`/mistralrs, which is in-process and never gets
        // here). A streaming remote provider would need its own handling --
        // a failure can land mid-stream, after bytes the caller already saw.
        HttpClientExt::send_streaming(&self.inner, req)
    }
}

/// The retry loop. A free function rather than a method because the future
/// `send` returns is `'static` and so may not borrow `&self`.
async fn send_with_retry<U>(
    client: reqwest::Client,
    policy: RetryPolicy,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> rig::http_client::Result<Response<LazyBody<U>>>
where
    U: From<Bytes> + WasmCompatSend + 'static,
{
    let url = uri.to_string();
    // `tokio::time::Instant`, not `std`, so `start_paused` tests see the
    // sleeps below advance the same clock this is measured against.
    let started = tokio::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        let sent = client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;

        let (err, retry_after) = match sent {
            Ok(response) if response.status().is_success() => return into_lazy_response(response),
            Ok(response) => {
                let status = response.status();
                // Read before `text()` consumes the response.
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| parse_retry_after(value, jiff::Timestamp::now()));
                // Mirrors rig's own `non_success_status_error` (keep in sync
                // with rig-core 0.40 `src/http_client/mod.rs:69-76`), so a
                // failure that escapes this loop is shaped exactly as it would
                // have been without the wrapper -- which is what keeps rig's
                // public `provider_response_body` helper working.
                //
                // Reading the body to completion also matters on the *retry*
                // path, where the message is dropped unused: abandoning a
                // partially-read body makes hyper drop the connection instead
                // of returning it to the pool, so the retry would pay a fresh
                // TCP and TLS handshake to save a few KiB of read.
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
                let err = HttpError::InvalidStatusCodeWithMessage(status, message);
                if !is_retryable_status(status) {
                    return Err(err);
                }
                (err, retry_after)
            }
            // Connection failures, read timeouts, and the like -- all
            // retry-worthy, and none of them carry a `Retry-After`.
            Err(error) => (HttpError::Instance(Box::new(error)), None),
        };

        let elapsed = started.elapsed();
        let Some(delay) = next_delay(&policy, attempt, elapsed, retry_after, jitter()) else {
            return Err(err);
        };
        // Kept under 100 columns for a realistic status line and budget, so a
        // wait does not wrap in a terminal the user is watching.
        eprintln!(
            "[outrig] LLM call failed ({}); retry in {:.1}s ({}; {}s/{}s spent)",
            failure_label(&err),
            delay.as_secs_f64(),
            if retry_after.is_some() {
                "Retry-After"
            } else {
                "backoff"
            },
            elapsed.as_secs(),
            policy.budget.as_secs(),
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// Repackage a successful `reqwest::Response` as the `http::Response` rig
/// expects, with the body left lazy. Mirrors rig's `HttpClientExt for
/// reqwest::Client` -- keep in sync with rig-core 0.40
/// `src/http_client/mod.rs:155-215`. rig exposes no public converter for this,
/// so it is a copy by necessity rather than by choice.
fn into_lazy_response<U>(
    response: reqwest::Response,
) -> rig::http_client::Result<Response<LazyBody<U>>>
where
    U: From<Bytes> + WasmCompatSend + 'static,
{
    let mut res = Response::builder().status(response.status());
    if let Some(hs) = res.headers_mut() {
        *hs = response.headers().clone();
    }
    let body: LazyBody<U> = Box::pin(async {
        let bytes = response
            .bytes()
            .await
            .map_err(|e| HttpError::Instance(e.into()))?;
        Ok(U::from(bytes))
    });
    res.body(body).map_err(HttpError::Protocol)
}

/// The retry-worthy statuses: request timeouts, "too early", rate limits, and
/// the server-side 5xx family. Everything else -- other 4xx especially -- is
/// terminal, because replaying it would fail the same way.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

/// Pre-jitter backoff in seconds: `base_delay * 2^attempt`, capped at
/// `max_delay`. Factored out so the (deterministic) schedule is testable.
fn backoff_secs(policy: &RetryPolicy, attempt: u32) -> f64 {
    let factor = 2f64.powi(attempt.min(16) as i32);
    (policy.base_delay.as_secs_f64() * factor).min(policy.max_delay.as_secs_f64())
}

/// Equal jitter: a random factor in `[0.5, 1.0]`, so a fleet of clients that
/// tripped the same limit does not retry in lockstep.
fn jitter() -> f64 {
    rand::rng().random_range(0.5..=1.0)
}

/// Parse a `Retry-After` value, which RFC 9110 gives in either of two forms:
/// delta-seconds (`"120"`) or an HTTP-date (`"Wed, 21 Oct 2015 07:28:00 GMT"`).
/// A date already in the past yields [`Duration::ZERO`]; anything unparseable
/// yields `None`, which sends the caller back to its backoff curve.
///
/// Only the IMF-fixdate form of an HTTP-date is read, which is what RFC 9110
/// requires a sender to emit. The two obsolete forms it still permits a
/// *recipient* to accept -- RFC 850 and asctime -- fall to `None` and so to the
/// backoff curve, which is a fine outcome for a header nobody sends that way.
///
/// `now` is a parameter rather than read here so the date branch is testable
/// without freezing the clock.
fn parse_retry_after(value: &str, now: jiff::Timestamp) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let deadline = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
    // A negative span means the date has passed: retry now, not never.
    Some(Duration::try_from(now.duration_until(deadline)).unwrap_or(Duration::ZERO))
}

/// How long to wait before the next attempt, or `None` to give up.
///
/// `jitter` is injected so the whole decision -- including the budget
/// arithmetic -- is unit-testable with no RNG and no sleeping.
fn next_delay(
    policy: &RetryPolicy,
    attempt: u32,
    elapsed: Duration,
    retry_after: Option<Duration>,
    jitter: f64,
) -> Option<Duration> {
    let remaining = policy.budget.checked_sub(elapsed)?;
    let delay = match retry_after {
        // No jitter on a server-named delay: the server told us when to come
        // back, and jittering *down* means hammering it early.
        Some(after) => after.min(MAX_RETRY_AFTER),
        None => Duration::from_secs_f64(backoff_secs(policy, attempt) * jitter),
    };
    let delay = delay.max(MIN_DELAY);
    // The remaining budget is a stop, not a truncation. Sleeping less than the
    // server asked burns the delay and retries too early, and there would be no
    // budget left for the attempt to finish anyway.
    (delay < remaining).then_some(delay)
}

/// A short label for a failure: the status line, or the transport error. Never
/// the provider's error body -- a rate-limited gateway's body runs to kilobytes
/// of nested JSON.
fn failure_label(err: &HttpError) -> String {
    match err {
        HttpError::InvalidStatusCode(code) | HttpError::InvalidStatusCodeWithMessage(code, _) => {
            format!("HTTP {code}")
        }
        HttpError::Instance(inner) => format!("connection error: {inner}"),
        other => other.to_string(),
    }
}

/// Classify an HTTP failure as transient -- the same predicate the retry loop
/// applies, so what the REPL calls recoverable is exactly what was retried.
fn is_transient(err: &HttpError) -> bool {
    match err {
        HttpError::InvalidStatusCode(code) | HttpError::InvalidStatusCodeWithMessage(code, _) => {
            is_retryable_status(*code)
        }
        HttpError::Instance(_) => true,
        _ => false,
    }
}

/// Did this turn fail transiently -- and so recoverably -- rather than for a
/// reason a second attempt could not fix? Returns the label to report if so.
///
/// Sound only because there is exactly one retry layer, and everything it can
/// retry it *did* retry until the budget stopped it. Adding a second retry
/// layer, or an early return for a retryable status in [`send_with_retry`],
/// would make this claim more than it knows. Note it stays true when the budget
/// is `0`: nothing was retried, and there was nothing to retry with.
///
/// Note the deliberate reach: a wrong `base-url` fails with a connection error,
/// which is transient by this predicate, so an unreachable endpoint ends the
/// turn rather than the session. That is right for a REPL -- the message names
/// the connection failure and the user can fix the config or `/quit` -- but it
/// does mean a typo no longer exits non-zero. Narrowing it is filed as
/// `plan/next/connect-failures-are-not-really-transient.md`.
///
/// Returning the label rather than a `bool` keeps the classification and the
/// thing to print together: a caller cannot decide this is recoverable without
/// also holding the reason to show.
pub fn exhausted_transient_label(err: &PromptError) -> Option<String> {
    match err {
        PromptError::CompletionError(CompletionError::HttpError(http)) if is_transient(http) => {
            Some(failure_label(http))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_status(code: u16) -> CompletionError {
        CompletionError::HttpError(HttpError::InvalidStatusCodeWithMessage(
            StatusCode::from_u16(code).unwrap(),
            "boom".to_string(),
        ))
    }

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().expect("fixed timestamp")
    }

    /// What the REPL sees for a given completion failure: the same wrapping the
    /// prompt loop does before `handle_prompt_error` classifies it.
    fn label_of(err: CompletionError) -> Option<String> {
        exhausted_transient_label(&PromptError::CompletionError(err))
    }

    #[test]
    fn retryable_status_codes_are_transient() {
        for code in [408, 425, 429, 500, 502, 503, 504] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(is_retryable_status(status), "{code} should retry");
            assert!(
                label_of(http_status(code)).is_some(),
                "{code} should be recoverable at the REPL too",
            );
        }
    }

    #[test]
    fn client_errors_are_terminal() {
        for code in [400, 401, 403, 404, 422] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!is_retryable_status(status), "{code} should not retry");
            assert!(
                label_of(http_status(code)).is_none(),
                "{code} should stay fatal at the REPL",
            );
        }
    }

    #[test]
    fn transport_errors_are_transient() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        let err = CompletionError::HttpError(HttpError::Instance(Box::new(io)));
        assert!(label_of(err).is_some());
    }

    #[test]
    fn non_http_errors_are_terminal() {
        assert!(label_of(CompletionError::ProviderError("nope".into())).is_none());
        assert!(label_of(CompletionError::ResponseError("nope".into())).is_none());
    }

    #[test]
    fn backoff_schedule_grows_then_saturates() {
        // 1, 2, 4, 8, 16, then capped at the policy's max_delay (30).
        let policy = RetryPolicy::default();
        assert_eq!(backoff_secs(&policy, 0), 1.0);
        assert_eq!(backoff_secs(&policy, 1), 2.0);
        assert_eq!(backoff_secs(&policy, 2), 4.0);
        let max = policy.max_delay.as_secs_f64();
        assert_eq!(backoff_secs(&policy, 5), max);
        assert_eq!(backoff_secs(&policy, 50), max);
        // Monotonic non-decreasing.
        for a in 0..20 {
            assert!(backoff_secs(&policy, a) <= backoff_secs(&policy, a + 1));
        }
    }

    #[test]
    fn backoff_jitter_stays_within_bounds() {
        let policy = RetryPolicy::default();
        for attempt in 0..=5 {
            let cap = backoff_secs(&policy, attempt);
            for _ in 0..100 {
                let d = next_delay(&policy, attempt, Duration::ZERO, None, jitter())
                    .expect("a fresh budget always allows the first backoff")
                    .as_secs_f64();
                assert!(d >= cap * 0.5 - f64::EPSILON, "{d} < {}", cap * 0.5);
                assert!(d <= cap + f64::EPSILON, "{d} > {cap}");
            }
        }
    }

    #[test]
    fn parse_retry_after_reads_delta_seconds() {
        let now = at("2015-10-21T07:28:00Z");
        assert_eq!(parse_retry_after("120", now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after(" 21 ", now), Some(Duration::from_secs(21)));
        assert_eq!(parse_retry_after("0", now), Some(Duration::ZERO));
        // Clamping is `next_delay`'s job, so an absurd delta still parses.
        assert_eq!(
            parse_retry_after("99999", now),
            Some(Duration::from_secs(99999))
        );
    }

    #[test]
    fn parse_retry_after_reads_http_dates() {
        let now = at("2015-10-21T07:28:00Z");
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:29:00 GMT", now),
            Some(Duration::from_secs(60)),
        );
        // Already past: retry now, not never.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn parse_retry_after_rejects_garbage() {
        let now = at("2015-10-21T07:28:00Z");
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
    }

    #[test]
    fn next_delay_prefers_retry_after_over_the_curve() {
        let policy = RetryPolicy::default();
        let delay = next_delay(
            &policy,
            0,
            Duration::ZERO,
            Some(Duration::from_secs(45)),
            0.5,
        );
        // 45s, unjittered and un-capped by max_delay (30s) -- the server's
        // instruction outranks our curve.
        assert_eq!(delay, Some(Duration::from_secs(45)));
    }

    #[test]
    fn next_delay_clamps_an_absurd_retry_after() {
        let policy = RetryPolicy {
            budget: Duration::from_secs(3600),
            ..RetryPolicy::default()
        };
        let delay = next_delay(
            &policy,
            0,
            Duration::ZERO,
            Some(Duration::from_secs(7200)),
            1.0,
        );
        assert_eq!(delay, Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn next_delay_floors_a_zero_delay() {
        let policy = RetryPolicy::default();
        let delay = next_delay(&policy, 0, Duration::ZERO, Some(Duration::ZERO), 1.0);
        assert_eq!(delay, Some(MIN_DELAY));
    }

    #[test]
    fn next_delay_stops_once_the_budget_is_spent() {
        let policy = RetryPolicy::default();
        assert_eq!(
            next_delay(&policy, 0, policy.budget, None, 1.0),
            None,
            "an exactly-spent budget stops",
        );
        assert_eq!(
            next_delay(&policy, 0, policy.budget + Duration::from_secs(1), None, 1.0),
            None,
            "an overspent budget stops",
        );
    }

    #[test]
    fn next_delay_stops_when_the_wait_would_not_fit() {
        let policy = RetryPolicy::default();
        // 9s left, and the server asked for 30: giving up beats sleeping 9s
        // and retrying into a window that has not reopened.
        let elapsed = policy.budget - Duration::from_secs(9);
        assert_eq!(
            next_delay(&policy, 0, elapsed, Some(Duration::from_secs(30)), 1.0),
            None,
        );
    }

    #[test]
    fn a_zero_budget_disables_retries() {
        let policy = RetryPolicy {
            budget: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_eq!(next_delay(&policy, 0, Duration::ZERO, None, 1.0), None);
    }

    #[test]
    fn exhausted_transient_label_names_the_status_without_the_body() {
        let label = label_of(http_status(429)).expect("a 429 is recoverable");
        assert!(label.contains("429"), "{label}");
        assert!(!label.contains("boom"), "the body must not leak: {label}");
    }

    #[test]
    fn exhausted_transient_label_ignores_terminal_failures() {
        assert!(label_of(http_status(401)).is_none());
        assert!(
            exhausted_transient_label(&PromptError::MaxTurnsError {
                max_turns: 3,
                chat_history: Box::new(Vec::new()),
                prompt: Box::new("hi".into()),
            })
            .is_none()
        );
    }
}
