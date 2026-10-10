//! Transient-error retry, in two layers.
//!
//! [`RetryingHttpClient`] handles failures the transport can see -- a status
//! code or a dead connection. [`RetryingModel`] handles the one it cannot: a
//! `200 OK` whose body carries no usable content, which is a success to the
//! transport and a [`CompletionError::ResponseError`] to rig.
//!
//! Both are safe to retry for the same reason. rig runs tools *between*
//! `completion()` calls, never inside one, so replaying either a single request
//! or a single model call re-executes no container tool call. Retrying the whole
//! `agent.prompt(...)` would -- a turn that fails on a *later* model call has
//! already run the tool calls from the earlier ones -- which is why neither
//! layer is up there.
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
//! That budget is really two, because "the endpoint answered badly" and "the
//! endpoint never answered at all" are not the same failure. Until some attempt
//! gets bytes back, the much shorter [`CONNECT_BUDGET`] applies: a host that
//! refuses every connection is usually a typo'd `base-url` or a dead address,
//! and neither heals in ten minutes. It is still long enough to ride out a load
//! balancer that is restarting, which is the case the retry exists for. Once
//! the endpoint has produced a response, the full budget applies for the rest
//! of that request -- a `503` followed by a failure to reconnect is a provider
//! having a bad minute, not an address that was never right. The short bound is
//! a fixed constant while the full one is `retry-budget-secs`; a zero budget
//! short-circuits both, so it stays the one "no retries" knob. The loop checks
//! the clock only between attempts, so [`CONNECT_TIMEOUT`] bounds the
//! connection itself -- otherwise a host that drops packets rather than
//! refusing them would sit in one attempt past either budget.
//!
//! Retrying transport failures *there* rather than around a
//! [`CompletionModel`] call is what makes `Retry-After` reachable at all: rig's
//! `http_client::Error` carries only a status code and a body string, so by the
//! time a failure has become a `CompletionError` the headers are gone.
//!
//! [`RetryingModel`] wraps the model itself and covers what is left: a response
//! rig could not turn into a completion. It takes the same budget, and an
//! attempt count on top, because an unusable body comes back immediately -- the
//! budget alone would spend itself on dozens of tries instead of the handful a
//! hiccup deserves. Sharing the budget keeps `retry-budget-secs = 0` the one
//! "no retries" knob.
//!
//! The obvious "just use the ecosystem crate" answer is `reqwest-retry`, which
//! rig itself carries as a dev-dependency. It does not fit: as of 0.9 it does
//! not read `Retry-After` at all, which is the feature this exists for.
//!
//! Copied from `outrig-cli`'s `llm/retry.rs`, which keeps its own. What changed
//! in coming across:
//!
//! - A library does not print. Each retry is a `tracing` warning, and a
//!   `model.retry` event in the agent's log ([`Retries`]).
//! - Each request sent is an attempt of its own, recorded once as a
//!   `model.attempt` event ([`super::ledger`]): this loop records those that
//!   fail, and [`RetryingModel`] those that came back `2xx`, since only it
//!   learns whether the body was usable.
//! - The predicates the CLI's REPL classifies a failed turn with are not here.
//!   A round's failure ends the round whatever it was, so nothing reads them;
//!   [`is_recoverable`] remains, for what a chain says of its exhaustion.
//!
//! [`CompletionModel`]: rig::completion::CompletionModel

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rand::RngExt;
use reqwest::StatusCode;
use rig::completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse};
use rig::http_client::{
    Error as HttpError, HeaderMap, HttpClientExt, LazyBody, Method, MultipartForm, Request,
    Response, StreamingResponse, Uri,
};
use rig::streaming::StreamingCompletionResponse;
use rig::wasm_compat::WasmCompatSend;

use super::ledger::CallSlot;
use crate::events::Events;
use crate::harness::event::{AttemptId, CallId, Payload};

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
/// Ceiling on the whole retry loop when the endpoint has never answered --
/// every attempt so far failed to connect, so nothing has come back from it.
/// Deliberately not configurable, like the delays above: it describes how long
/// a restarting load balancer takes to come back, not a preference. A config
/// key can be added later additively; the reverse is not true.
///
/// Thirty seconds is a guess, bounded on one side by that restart and on the
/// other by a user's patience, and neither is measured. It is long enough for
/// the backoff curve to spend four or five attempts on a provider that is
/// briefly down, and nowhere near long enough to sit through a typo'd
/// `base-url`. Revisit it with a number rather than an intuition.
const CONNECT_BUDGET: Duration = Duration::from_secs(30);
/// Ceiling on one attempt's *connection* -- DNS, TCP, TLS -- handed to the
/// `reqwest` client that [`RetryingHttpClient`] wraps.
///
/// [`CONNECT_BUDGET`] alone does not bound a host that drops packets rather
/// than refusing them: the loop checks the clock between attempts, so a SYN
/// nobody answers would sit in `send` until `request-timeout-secs` -- ten
/// minutes by default -- and only then find the budget spent. Capping the
/// connection is what makes the short budget a real ceiling on an address that
/// was never right.
///
/// Deliberately *not* a cap on the whole request. A non-streaming completion
/// returns its headers when the model has finished generating, so the wait for
/// a response is indistinguishable from a model thinking hard, and bounding it
/// here would cut off exactly the long reasoning turns
/// `request-timeout-secs` defaults high to protect. This bounds only the phase
/// before there is anything to wait for.
///
/// Small enough that [`CONNECT_BUDGET`] buys more than one try at a gateway
/// that is restarting -- pinned by a test, since the two constants are only
/// useful in proportion to each other.
///
/// It is one flat cap, not a slice of whatever budget is left, and both are
/// deliberate. reqwest sets a connect timeout per *client*, not per request, so
/// a bound that tracked the remaining budget would mean rebuilding the client
/// mid-request and throwing away its connection pool. The consequences are
/// worth naming: a budget smaller than this does not shrink it -- the same rule
/// [`RetryPolicy::budget`] already follows, where an attempt in flight is never
/// cancelled by the clock running out -- so the loop stops *retrying* at its
/// bound and the last attempt can run past it by up to this much. It also
/// applies after `answered` latches, where the full budget is otherwise in
/// force: a reconnect whose handshake takes longer than this fails, though as a
/// connect failure it is transient and simply retried.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra attempts [`RetryingModel`] spends on an unusable response, on top of
/// the first, and on top of the budget it shares with the HTTP loop. Small on
/// purpose: each one resends the whole conversation, and a body that is
/// unusable three times running is not a hiccup.
const RESPONSE_RETRY_ATTEMPTS: u32 = 2;

/// A deadline shared by every candidate in one failover chain.
///
/// The chain's problem is that its bound cannot be a per-candidate one. Three
/// candidates at the default `retry-budget-secs = 600` is a thirty-minute turn
/// against a total outage, and most of that is spent retrying endpoints already
/// known to be down -- a worst case worse than no failover at all. Splitting the
/// budget `N` ways instead shrinks it in the case that matters most: candidate
/// one instantly dead, candidate two deserving the whole thing.
///
/// So the bound is one wall-clock instant, armed once per `completion()` call by
/// [`FailoverModel`] and consulted by every candidate's retry loop underneath
/// it. The handle is shared rather than copied because the arming happens above
/// the candidates and has to be visible inside them -- through rig's
/// `HttpClientExt::send`, which has no per-call context channel of its own.
///
/// [`FailoverModel`]: super::failover::FailoverModel
#[derive(Debug, Default)]
pub(crate) struct ChainDeadline {
    /// `None` until a chain arms it. Every agent's model is a chain, of one
    /// model or more, so only a policy outside one -- rig's `Default` and
    /// `make` paths, and tests -- stays unarmed, with the per-attempt budgets
    /// its only bound.
    at: Mutex<Option<tokio::time::Instant>>,
}

impl ChainDeadline {
    /// Start the clock: `budget` from now, for whatever the chain does next.
    ///
    /// Re-armed on every `completion()` call, which is what makes the bound
    /// per-call rather than per-session. A turn is a sequence of calls with tool
    /// calls between them, and each call gets a whole budget to find a working
    /// candidate -- bounding the turn instead would make a long agentic turn's
    /// last model call inherit a budget its first one spent.
    pub(crate) fn arm(&self, budget: Duration) {
        *self.at.lock().expect("chain deadline") = Some(tokio::time::Instant::now() + budget);
    }

    /// How long is left, or `None` when no deadline is armed.
    ///
    /// `Some(Duration::ZERO)` once it has passed; the `Option` distinguishes
    /// armed from unarmed, never expired from live.
    fn remaining(&self) -> Option<Duration> {
        let at = (*self.at.lock().expect("chain deadline"))?;
        Some(at.saturating_duration_since(tokio::time::Instant::now()))
    }
}

/// Knobs for one client's retry loop.
///
/// Not `Copy`: it carries an `Arc` into each `'static` per-request future.
/// That `Arc` is [`chain_deadline`], the bound a failover chain shares across
/// its candidates -- a handle rather than a value precisely because the arming
/// happens outside the candidate that must observe it.
/// Cloning stays cheap (four `Duration`s and a refcount bump), and a clone
/// deliberately *shares* the deadline rather than copying it, which is the
/// whole point of the indirection.
///
/// [`chain_deadline`]: Self::chain_deadline
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    /// Wall clock from the first attempt's start, including the time each
    /// attempt spends in flight -- not only the time spent sleeping. Zero
    /// disables retries.
    pub(crate) budget: Duration,
    /// The same wall clock, but the ceiling that applies while the endpoint has
    /// never answered -- see [`CONNECT_BUDGET`]. Much shorter than [`budget`]:
    /// a host that refuses every connection is usually misconfigured, not busy.
    ///
    /// A bound in its own right rather than a mode of [`budget`], so a reader
    /// -- or a failover chain picking its next candidate -- can consume it
    /// without knowing which request state produced it. Never exceeds
    /// [`budget`] in effect: the loop applies whichever is smaller, so a budget
    /// of zero still means no retries anywhere.
    ///
    /// [`budget`]: Self::budget
    pub(crate) connect_budget: Duration,
    /// First backoff delay, doubling each attempt.
    pub(crate) base_delay: Duration,
    /// Ceiling on one backoff delay. Does not cap a server's `Retry-After`.
    pub(crate) max_delay: Duration,
    /// The failover chain's shared bound, when this policy belongs to one.
    ///
    /// It costs an uncontended lock and an `Option` check per delay decision --
    /// on a path that then sleeps for at least `MIN_DELAY`. See
    /// [`ChainDeadline`].
    pub(crate) chain_deadline: Arc<ChainDeadline>,
}

impl RetryPolicy {
    /// The ceiling in force for a request in the given state: the full
    /// [`budget`] once the endpoint has answered, and while it has not, the
    /// *smaller* of the two bounds rather than [`connect_budget`] outright -- a
    /// zero `budget` is the documented "no retries" knob, so it has to
    /// short-circuit this path too rather than acquire an exception.
    ///
    /// One method rather than the rule spelled out at each reader, because the
    /// loop both decides against it and prints it, and the two must agree.
    ///
    /// A duration measured from this request's first attempt, which is what
    /// makes it the denominator the retry line prints and the value [`left`]
    /// subtracts elapsed time from. The chain deadline is deliberately *not*
    /// folded in here: it counts down in absolute time and so is already net of
    /// elapsed time, and mixing the two would subtract elapsed twice -- halving
    /// the effective budget and moving off the preferred candidate early.
    ///
    /// [`budget`]: Self::budget
    /// [`connect_budget`]: Self::connect_budget
    /// [`left`]: Self::left
    fn bound(&self, answered: bool) -> Duration {
        if answered {
            self.budget
        } else {
            self.budget.min(self.connect_budget)
        }
    }

    /// How much of [`bound`] is left after `elapsed`, capped by the chain
    /// deadline when one is armed.
    ///
    /// Two quantities that must not be confused. `elapsed` counts against
    /// *this request's* own budget, so it is subtracted from it. The chain
    /// deadline is an absolute instant shared across candidates, so it is
    /// already counting down on its own and must be compared against rather
    /// than reduced by `elapsed`. Whichever remainder is smaller wins, which is
    /// what makes a chain's worst case one budget rather than one per
    /// candidate, and keeps `budget = 0` meaning no retries anywhere -- the
    /// minimum of zero and anything is still zero.
    ///
    /// [`bound`]: Self::bound
    fn left(&self, answered: bool, elapsed: Duration) -> Duration {
        let own_left = self.bound(answered).saturating_sub(elapsed);
        match self.chain_deadline.remaining() {
            Some(deadline_left) => own_left.min(deadline_left),
            None => own_left,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            budget: Duration::from_secs(crate::config::DEFAULT_RETRY_BUDGET_SECS),
            connect_budget: CONNECT_BUDGET,
            base_delay: BASE_DELAY,
            max_delay: MAX_DELAY,
            chain_deadline: Arc::default(),
        }
    }
}

/// Where one candidate's attempts and retries are recorded: the chain's
/// [`CallSlot`], under the `[models.<name>]` row being retried.
///
/// Both layers hold one. The HTTP loop's future is `'static`, so this is
/// cloned into it, and [`Events::emit`] never waits, so recording from inside
/// a retry loop cannot hold it up. `Default` records nowhere, which is what
/// rig's `Default` and `make` paths get.
#[derive(Clone, Default)]
pub(crate) struct Retries {
    slot: CallSlot,
    model: Arc<str>,
}

impl Retries {
    /// Retries of `model`'s calls, recorded through `slot`.
    pub(crate) fn new(slot: CallSlot, model: &str) -> Self {
        Self {
            slot,
            model: Arc::from(model),
        }
    }

    fn events(&self) -> &Events {
        self.slot.ledger().events()
    }

    /// Record that try `attempt`, counted from 1, of `call` -- the request
    /// `attempt_id` -- failed with `error`, and that the next is `delay` away.
    fn record(
        &self,
        attempt: u32,
        delay: Duration,
        error: &str,
        (call_id, attempt_id): (CallId, AttemptId),
    ) {
        self.events().emit(Payload::ModelRetry {
            model: self.model.to_string(),
            attempt,
            delay,
            error: error.to_string(),
            call_id,
            attempt_id,
        });
    }
}

/// By hand, since the log has nothing to show: rig's builder wants `Debug` on
/// the HTTP client.
impl fmt::Debug for Retries {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Retries")
            .field("model", &self.model)
            .field("recorded", &self.events().is_on())
            .finish()
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
pub(crate) struct RetryingHttpClient {
    inner: reqwest::Client,
    policy: RetryPolicy,
    retries: Retries,
}

impl RetryingHttpClient {
    pub(crate) fn new(inner: reqwest::Client, policy: RetryPolicy, retries: Retries) -> Self {
        Self {
            inner,
            policy,
            retries,
        }
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
        // Cloned, not copied: the policy carries the chain's shared deadline
        // handle, and the clone shares it rather than duplicating it.
        let policy = self.policy.clone();
        let (parts, body) = req.into_parts();
        // Converted once: `Bytes::clone` is a refcount bump, so replaying the
        // body on each attempt costs nothing.
        let body: Bytes = body.into();
        let retries = self.retries.clone();
        send_with_retry(
            client,
            policy,
            retries,
            parts.method,
            parts.uri,
            parts.headers,
            body,
        )
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
        // No retry: outrig never streams -- every turn is a non-streaming
        // completion, so nothing reaches here. A streaming provider would need
        // its own handling -- a failure can land mid-stream, after bytes the
        // caller already saw.
        HttpClientExt::send_streaming(&self.inner, req)
    }
}

/// A [`CompletionModel`] that retries a response rig could not use, which a
/// chain's candidate holds in place of the provider's own model.
///
/// The failure it exists for is a provider answering `200 OK` with no usable
/// content blocks. rig rejects that in its `TryFrom<CompletionResponse>` --
/// `CompletionError::ResponseError("Response contained no message or tool call
/// (empty)")` -- and it used to end the session six turns into a conversation.
/// [`RetryingHttpClient`] cannot see it: a `200` is a success down there.
///
/// The whole `ResponseError` variant is retried, not that one message. The
/// variant means exactly "the provider's body could not be turned into a
/// completion", which is always the provider's doing and never a local
/// misconfiguration, and matching the variant leaves no wording to keep in sync
/// with rig.
///
/// A model-layer wrapper is what `deedd83` deliberately removed when the
/// transient retry moved down to [`RetryingHttpClient`], and it brings back the
/// per-call `CompletionRequest` clone that commit was glad to lose. It earns it
/// by catching a class that layer cannot see at all, and the clone is skipped
/// entirely when retries are off. Against the JSON serialization of that same
/// request on the very next line, one clone is the cheaper half.
#[derive(Clone, Debug)]
pub(crate) struct RetryingModel<M> {
    inner: M,
    policy: RetryPolicy,
    retries: Retries,
}

impl<M> RetryingModel<M> {
    pub(crate) fn new(inner: M, policy: RetryPolicy, retries: Retries) -> Self {
        Self {
            inner,
            policy,
            retries,
        }
    }
}

impl<M: CompletionModel> CompletionModel for RetryingModel<M> {
    type Response = M::Response;
    type StreamingResponse = M::StreamingResponse;
    type Client = M::Client;

    /// Never reached by outrig -- `build_agent` wraps a model the provider
    /// client already made -- but rig's trait requires it, so it wraps a model
    /// made the same way against the shipped policy, recording nowhere.
    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self::new(
            M::make(client, model),
            RetryPolicy::default(),
            Retries::default(),
        )
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Read off the request before rig consumes it: naming the ceiling that
        // was in force is most of what makes a truncation report actionable,
        // and `None` -- no ceiling sent, provider's default silently in charge
        // -- is the case worth naming loudest.
        let max_tokens = request.max_tokens;
        let outcome = self.completion_retried(request).await.0;
        if let Ok(response) = &outcome {
            report_textless_completion(&self.retries.model, response, max_tokens);
        }
        outcome
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        // No retry, for the same reason `send_streaming` has none: outrig
        // never streams, and a failure can land mid-stream, after content the
        // caller already saw.
        self.inner.stream(request).await
    }

    /// Delegated, not defaulted. The trait's `false` is the safe answer for a
    /// provider whose native structured output suppresses tool calls; answering
    /// it for OpenAI and Anthropic, which compose the two, would cost them
    /// guaranteed structured output on every turn that has tools.
    fn composes_native_output_with_tools(&self) -> bool {
        self.inner.composes_native_output_with_tools()
    }
}

impl<M: CompletionModel> RetryingModel<M> {
    /// One completion from the inner model, and the attempt it settled, once
    /// recorded: its usage if rig made a completion of the body, and why not
    /// if it could not.
    async fn attempt(
        &self,
        request: CompletionRequest,
    ) -> (
        Result<CompletionResponse<M::Response>, CompletionError>,
        Option<(CallId, AttemptId)>,
    ) {
        let settling = self.retries.slot.awaiting();
        let outcome = self.inner.completion(request).await;
        let ids = settling.settle(&outcome);
        (outcome, ids)
    }

    /// The retry loop proper. Split out so [`CompletionModel::completion`] can
    /// inspect what came back without the reporting having to live inside the
    /// loop and fire once per attempt.
    async fn completion_retried(
        &self,
        request: CompletionRequest,
    ) -> (
        Result<CompletionResponse<M::Response>, CompletionError>,
        Option<(CallId, AttemptId)>,
    ) {
        // rig takes the request by value, so replaying one means holding a copy
        // of the whole conversation for the duration of the call. With retries
        // off there is nothing to replay, so the wrapper costs nothing at all.
        if self.policy.budget.is_zero() {
            return self.attempt(request).await;
        }

        // `tokio::time::Instant` for the same reason `send_with_retry` uses it:
        // the sleeps below advance the clock a `start_paused` test measures
        // against.
        let started = tokio::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            let (message, ids) = match self.attempt(request.clone()).await {
                (Err(CompletionError::ResponseError(message)), ids) => (message, ids),
                // A success, or a failure this layer cannot fix: one the HTTP
                // client already retried, or a terminal one -- a bad key's
                // `401` above all, which no number of tries will fix.
                outcome => return outcome,
            };

            // Bounded by a count *and* by the budget, unlike the HTTP loop. An
            // unusable response is not a condition that clears with waiting --
            // the provider answered, it just answered with nothing -- so
            // spending ten minutes of budget re-rolling it would only delay
            // telling the user.
            //
            // `answered: true` unconditionally: reaching this layer at all
            // means a `200 OK` came back, so the connect bound cannot apply.
            let delay = (attempt < RESPONSE_RETRY_ATTEMPTS)
                .then(|| {
                    next_delay(
                        &self.policy,
                        attempt,
                        started.elapsed(),
                        true,
                        None,
                        jitter(),
                    )
                })
                .flatten();
            let Some(delay) = delay else {
                return (Err(CompletionError::ResponseError(message)), ids);
            };

            tracing::warn!(
                "{} returned an unusable response ({message}); retry in {:.1}s (attempt {}/{})",
                self.retries.model,
                delay.as_secs_f64(),
                attempt + 2,
                RESPONSE_RETRY_ATTEMPTS + 1,
            );
            if let Some(ids) = ids {
                self.retries
                    .record(attempt + 1, delay, &unusable(&message), ids);
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// Why a `200` rig could not make a completion of failed, as an attempt and
/// its retry both record it.
pub(crate) fn unusable(message: &str) -> String {
    format!("unusable response ({message})")
}

/// Report a completion that came back with nothing to show for itself.
///
/// A response carrying no text and no tool call is a turn the user paid for and
/// cannot see. Rig treats it as an ordinary success -- `output` is the text
/// parts concatenated, so a reasoning-only turn is simply the empty string --
/// and every layer above here has already lost the provider's own account of
/// what happened. This is the last point that still holds it.
///
/// Deliberately keyed on the *decoded* content rather than on the finish
/// reason: that check is free on every turn, and the raw response is only
/// serialized in the rare case that already went wrong.
fn report_textless_completion<R: serde::Serialize>(
    model: &str,
    response: &CompletionResponse<R>,
    max_tokens: Option<u64>,
) {
    // An empty text part is not content: it is the sentinel rig normalizes an
    // empty provider turn into, so treating it as "the model spoke" is exactly
    // the mistake this function exists to catch.
    let showed_something = response.choice.iter().any(|part| match part {
        rig::message::AssistantContent::Text(text) => !text.text.trim().is_empty(),
        rig::message::AssistantContent::ToolCall(_) => true,
        _ => false,
    });
    if showed_something {
        return;
    }

    let ceiling = match max_tokens {
        Some(limit) => format!("the request carried max-tokens = {limit}"),
        None => {
            "the request carried no max-tokens, so the provider's own default applied".to_string()
        }
    };
    match provider_finish_reason(&response.raw_response) {
        Some(reason) => tracing::warn!(
            "{model} produced no text and no tool call this turn (provider finish reason: \
             {reason:?}); {ceiling}."
        ),
        None => {
            tracing::warn!("{model} produced no text and no tool call this turn; {ceiling}.")
        }
    }
}

/// The provider's own word for why generation stopped.
///
/// Rig parses this out of the wire and then drops it before any type outrig
/// sees, so the only way back to it is the raw response the completion still
/// carries. Both dialects are checked because both are reachable:
/// `choices[].finish_reason` is the OpenAI shape and `stop_reason` the
/// Anthropic one. Returns `None` for a provider that names neither, which
/// costs the report one clause and nothing else.
fn provider_finish_reason<R: serde::Serialize>(raw: &R) -> Option<String> {
    let value = serde_json::to_value(raw).ok()?;
    let openai = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("finish_reason"));
    openai
        .or_else(|| value.get("stop_reason"))
        .and_then(|reason| reason.as_str())
        .map(str::to_string)
}

/// The retry loop. A free function rather than a method because the future
/// `send` returns is `'static` and so may not borrow `&self`. Each retry is
/// recorded in `retries`.
async fn send_with_retry<U>(
    client: reqwest::Client,
    policy: RetryPolicy,
    retries: Retries,
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
    // Whether any attempt has got bytes back from the endpoint. Latches on and
    // never clears: once the endpoint has answered, a later failure to
    // reconnect is a provider having a bad minute rather than an address that
    // was never right, so the full budget applies for the rest of the request.
    let mut answered = false;
    loop {
        // An attempt from here: one that is dropped in flight -- its round
        // dropped -- records itself as cancelled.
        let pending = retries.slot.sending();
        let sent = client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;

        let (err, retry_after) = match sent {
            Ok(response) if response.status().is_success() => {
                if let Some(pending) = pending {
                    pending.answered();
                }
                return into_lazy_response(response);
            }
            Ok(response) => {
                answered = true;
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
                    settle_failed(pending, &err);
                    return Err(err);
                }
                (err, retry_after)
            }
            // A request that cannot succeed as sent fails the same way however
            // often it is sent, so it is final at once rather than spending the
            // budget -- and with it a failover chain's -- on replays.
            Err(error) if is_permanent(&error) => {
                let err = HttpError::Instance(Box::new(error));
                settle_failed(pending, &err);
                return Err(err);
            }
            // Read timeouts, connection resets, and the like -- all
            // retry-worthy, and none of them carry a `Retry-After`.
            Err(error) => {
                // Read here because the box below erases the concrete type, so
                // this is the only place a connect failure is still knowable.
                answered |= !error.is_connect();
                (HttpError::Instance(Box::new(error)), None)
            }
        };

        let label = failure_label(&err);
        let ids = pending.map(|pending| pending.failed(label.clone()));
        let elapsed = started.elapsed();
        let Some(delay) = next_delay(&policy, attempt, elapsed, answered, retry_after, jitter())
        else {
            return Err(err);
        };
        // The budget shown is the one actually being spent against, so a
        // connect failure does not count down against a ten-minute bound it
        // will never reach.
        tracing::warn!(
            "LLM call to {} failed ({label}); retry in {:.1}s ({}; {}s/{}s spent)",
            retries.model,
            delay.as_secs_f64(),
            if retry_after.is_some() {
                "Retry-After"
            } else {
                "backoff"
            },
            elapsed.as_secs(),
            policy.bound(answered).as_secs(),
        );
        if let Some(ids) = ids {
            retries.record(attempt + 1, delay, &label, ids);
        }
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// Record the attempt `pending` as failed with `err`, when it is one.
fn settle_failed(
    pending: Option<super::ledger::Pending>,
    err: &HttpError,
) -> Option<(CallId, AttemptId)> {
    pending.map(|pending| pending.failed(failure_label(err)))
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
/// the server-side 5xx family, Anthropic's `529` -- its documented, temporary
/// `overloaded_error` -- included. Everything else -- other 4xx especially --
/// is terminal, because replaying it would fail the same way.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 529
    )
}

/// A transport error that no replay of the same request can clear: one reqwest
/// could not build, or a redirect its policy refused to follow -- a loop that
/// reached its limit, say. Every other transport error is a timeout, a dropped
/// connection, or the like, which can.
fn is_permanent(error: &reqwest::Error) -> bool {
    error.is_builder() || error.is_redirect()
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
/// arithmetic -- is unit-testable with no RNG and no sleeping. `answered` says
/// whether any attempt has produced a response, which picks the bound:
/// see [`RetryPolicy::bound`].
fn next_delay(
    policy: &RetryPolicy,
    attempt: u32,
    elapsed: Duration,
    answered: bool,
    retry_after: Option<Duration>,
    jitter: f64,
) -> Option<Duration> {
    let remaining = policy.left(answered, elapsed);
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
/// applies, so what a chain calls recoverable is exactly what was retried.
fn is_transient(err: &HttpError) -> bool {
    match err {
        HttpError::InvalidStatusCode(code) | HttpError::InvalidStatusCodeWithMessage(code, _) => {
            is_retryable_status(*code)
        }
        HttpError::Instance(inner) => !inner
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_permanent),
        _ => false,
    }
}

/// Could this failure clear without a change to the config?
///
/// A transient `HttpError`, which the HTTP loop retried until its budget
/// stopped it, and a `ResponseError`, which [`RetryingModel`] re-rolled until
/// its attempts ran out. Everything else -- a `401`, a model that does not
/// exist, a provider-reported fault -- is terminal: no number of resends
/// satisfies it.
///
/// A failover chain asks this of each candidate's error before aggregating
/// them, so its report can say whether waiting is worth anything. Defined here,
/// beside the loops whose classes it unions, so the chain's verdict on an error
/// cannot drift from what was retried.
///
/// A wrong `base-url` fails with a connection error, which is transient by
/// this predicate. The wait before it is bounded by [`CONNECT_BUDGET`], so a
/// typo costs seconds rather than minutes.
pub(crate) fn is_recoverable(err: &CompletionError) -> bool {
    match err {
        CompletionError::HttpError(http) => is_transient(http),
        CompletionError::ResponseError(_) => true,
        _ => false,
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

    #[test]
    fn retryable_status_codes_are_transient() {
        for code in [408, 425, 429, 500, 502, 503, 504, 529] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(is_retryable_status(status), "{code} should retry");
            assert!(
                is_recoverable(&http_status(code)),
                "{code} should be recoverable to a chain too",
            );
        }
    }

    #[test]
    fn client_errors_are_terminal() {
        for code in [400, 401, 403, 404, 422] {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!is_retryable_status(status), "{code} should not retry");
            assert!(
                !is_recoverable(&http_status(code)),
                "{code} should stay terminal to a chain",
            );
        }
    }

    #[test]
    fn transport_errors_are_transient() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        let err = CompletionError::HttpError(HttpError::Instance(Box::new(io)));
        assert!(is_recoverable(&err));
    }

    /// What either loop retried is recoverable, and nothing else is: a
    /// provider-reported fault may well be a misconfiguration no retry and no
    /// resend can fix.
    #[test]
    fn only_what_a_loop_retries_is_recoverable() {
        assert!(is_recoverable(&CompletionError::ResponseError(
            "empty".into()
        )));
        assert!(is_recoverable(&http_status(503)));
        assert!(!is_recoverable(&CompletionError::ProviderError(
            "nope".into()
        )));
    }

    /// A model that fails the way the bug does, every time, counting the calls
    /// it took. The associated types are the trait's bare minimum: it never
    /// returns a response and never streams.
    /// A model that answers every request `200` with nothing rig can use, and
    /// counts them. It stands in for the HTTP layer too: each request it
    /// answers is one `slot` names.
    #[derive(Clone)]
    struct AlwaysUnusableModel(std::sync::Arc<std::sync::atomic::AtomicUsize>, CallSlot);

    impl CompletionModel for AlwaysUnusableModel {
        type Response = ();
        type StreamingResponse = ();
        type Client = ();

        fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
            Self(Default::default(), CallSlot::default())
        }

        async fn completion(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(sent) = self.1.sending() {
                sent.answered();
            }
            Err(CompletionError::ResponseError(
                "Response contained no message or tool call (empty)".into(),
            ))
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
            unreachable!("outrig never streams")
        }
    }

    /// The count is what has to stop a provider answering empty forever: an
    /// unusable response comes back in milliseconds, so on the default
    /// ten-minute budget the backoff curve alone would keep re-rolling for the
    /// whole of it with the user watching. `start_paused` means the waits cost
    /// this test no wall clock.
    #[tokio::test(start_paused = true)]
    async fn an_endlessly_unusable_response_gives_up_after_a_bounded_number_of_tries() {
        let calls: std::sync::Arc<std::sync::atomic::AtomicUsize> = std::sync::Arc::default();
        let model = RetryingModel::new(
            AlwaysUnusableModel(std::sync::Arc::clone(&calls), CallSlot::default()),
            RetryPolicy::default(),
            Retries::default(),
        );

        let err = model
            .completion(model.completion_request("hi").build())
            .await
            .expect_err("a model that only ever answers empty never recovers");

        assert!(matches!(err, CompletionError::ResponseError(_)), "{err}");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1 + RESPONSE_RETRY_ATTEMPTS as usize,
            "the first try plus the bounded re-rolls, and not one more",
        );
    }

    /// With retries off the wrapper is a pass-through: one call, no clone of
    /// the request, no wait.
    #[tokio::test]
    async fn a_zero_budget_makes_the_first_unusable_response_final() {
        let calls: std::sync::Arc<std::sync::atomic::AtomicUsize> = std::sync::Arc::default();
        let model = RetryingModel::new(
            AlwaysUnusableModel(std::sync::Arc::clone(&calls), CallSlot::default()),
            RetryPolicy {
                budget: Duration::ZERO,
                ..RetryPolicy::default()
            },
            Retries::default(),
        );

        model
            .completion(model.completion_request("hi").build())
            .await
            .expect_err("the first failure is final");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Each re-roll is recorded as the model's, with the attempt that failed,
    /// the wait before the next, and why -- not only announced. On the real
    /// clock: the log's writer is a task doing file I/O, which a paused clock
    /// would race. The floor on a delay keeps it to a fifth of a second.
    #[tokio::test]
    async fn each_retry_is_recorded() {
        let dir = tempfile::tempdir().expect("a log dir");
        let events = crate::events::opened(dir.path()).await;
        let slot = CallSlot::new(crate::agent::ledger::Ledger::new(events.clone()));
        slot.begin_call();
        slot.candidate("sonnet", "claude-sonnet-4-6", Some(4096));
        let model = RetryingModel::new(
            AlwaysUnusableModel(std::sync::Arc::default(), slot.clone()),
            RetryPolicy {
                base_delay: Duration::from_millis(1),
                ..RetryPolicy::default()
            },
            Retries::new(slot, "sonnet"),
        );

        model
            .completion(model.completion_request("hi").build())
            .await
            .expect_err("a model that only ever answers empty never recovers");
        events.close().await.expect("the log is finished");

        let records = crate::events::recorded(dir.path());
        let retries = crate::events::of_kind(&records, "model.retry");
        let floor = MIN_DELAY.as_secs_f64();
        let expected: Vec<serde_json::Value> = (1..=RESPONSE_RETRY_ATTEMPTS)
            .map(|attempt| {
                serde_json::json!({
                    "model": "sonnet",
                    "attempt": attempt,
                    "delay": floor,
                    "error": "unusable response (Response contained no message or tool call \
                              (empty))",
                    "call_id": 1,
                    "attempt_id": attempt,
                })
            })
            .collect();
        assert_eq!(retries, expected.iter().collect::<Vec<_>>());
        // Each try was a request of its own, of the one call, and none reported
        // what it used.
        let attempts = crate::events::of_kind(&records, "model.attempt");
        assert_eq!(attempts.len(), 1 + RESPONSE_RETRY_ATTEMPTS as usize);
        for (n, attempt) in attempts.iter().enumerate() {
            assert_eq!(attempt["call_id"], 1);
            assert_eq!(attempt["attempt_id"], n + 1);
            assert_eq!(attempt["model"], "sonnet");
            assert_eq!(attempt["identifier"], "claude-sonnet-4-6");
            assert_eq!(attempt["max_tokens"], 4096);
            assert_eq!(attempt["usage"], serde_json::Value::Null);
        }
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
                let d = next_delay(&policy, attempt, Duration::ZERO, true, None, jitter())
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
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after(" 21 ", now),
            Some(Duration::from_secs(21))
        );
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
            true,
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
            true,
            Some(Duration::from_secs(7200)),
            1.0,
        );
        assert_eq!(delay, Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn next_delay_floors_a_zero_delay() {
        let policy = RetryPolicy::default();
        let delay = next_delay(&policy, 0, Duration::ZERO, true, Some(Duration::ZERO), 1.0);
        assert_eq!(delay, Some(MIN_DELAY));
    }

    #[test]
    fn next_delay_stops_once_the_budget_is_spent() {
        let policy = RetryPolicy::default();
        assert_eq!(
            next_delay(&policy, 0, policy.budget, true, None, 1.0),
            None,
            "an exactly-spent budget stops",
        );
        assert_eq!(
            next_delay(
                &policy,
                0,
                policy.budget + Duration::from_secs(1),
                true,
                None,
                1.0
            ),
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
            next_delay(
                &policy,
                0,
                elapsed,
                true,
                Some(Duration::from_secs(30)),
                1.0
            ),
            None,
        );
    }

    #[test]
    fn a_zero_budget_disables_retries() {
        let policy = RetryPolicy {
            budget: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_eq!(
            next_delay(&policy, 0, Duration::ZERO, true, None, 1.0),
            None
        );
    }

    /// The point of the whole task: an endpoint that has never answered is
    /// bounded by the short budget, so a typo'd `base-url` gives up in seconds.
    #[test]
    fn an_unanswered_endpoint_gives_up_on_the_connect_budget() {
        let policy = RetryPolicy::default();
        // Comfortably inside the full budget (600s) and past the short one.
        let elapsed = policy.connect_budget + Duration::from_secs(1);
        assert!(
            elapsed < policy.budget,
            "fixture must sit between the two bounds"
        );
        assert_eq!(next_delay(&policy, 0, elapsed, false, None, 1.0), None);
        // The same elapsed time, once the endpoint has answered, keeps going.
        assert!(next_delay(&policy, 0, elapsed, true, None, 1.0).is_some());
    }

    /// A read timeout is not a connect failure: the acceptance criterion that
    /// distinguishes this from "every transport error is short-bounded".
    #[test]
    fn an_answered_endpoint_keeps_the_full_budget() {
        let policy = RetryPolicy::default();
        for elapsed in [
            policy.connect_budget,
            policy.connect_budget * 2,
            // Not `budget - 1s`: a wait that does not fit in what is left is
            // refused rather than truncated, which is the rule
            // `next_delay_stops_when_the_wait_would_not_fit` pins.
            policy.budget - policy.max_delay - Duration::from_secs(1),
        ] {
            assert!(
                next_delay(&policy, 0, elapsed, true, None, 1.0).is_some(),
                "{elapsed:?} is inside the full budget and must still retry"
            );
        }
    }

    /// A connect failure *before* the short bound is spent still retries --
    /// the restarting-load-balancer case the short bound is sized for.
    #[test]
    fn an_unanswered_endpoint_still_retries_inside_the_connect_budget() {
        let policy = RetryPolicy::default();
        assert!(next_delay(&policy, 0, Duration::ZERO, false, None, 1.0).is_some());
    }

    /// `retry-budget-secs = 0` is the one "no retries" knob, so it has to
    /// short-circuit the connect path too rather than acquire an exception.
    #[test]
    fn a_zero_budget_disables_retries_on_the_connect_path_too() {
        let policy = RetryPolicy {
            budget: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert!(
            !policy.connect_budget.is_zero(),
            "the short bound is non-zero, so this proves the budget wins"
        );
        assert_eq!(
            next_delay(&policy, 0, Duration::ZERO, false, None, 1.0),
            None
        );
    }

    /// A budget shorter than the connect bound is not widened by it: the loop
    /// applies whichever is smaller, in both directions.
    #[test]
    fn a_budget_below_the_connect_bound_still_wins() {
        let policy = RetryPolicy {
            budget: Duration::from_secs(5),
            ..RetryPolicy::default()
        };
        let elapsed = Duration::from_secs(6);
        assert!(elapsed < policy.connect_budget);
        assert_eq!(next_delay(&policy, 0, elapsed, false, None, 1.0), None);
    }

    /// A policy no chain armed is bounded by its own two budgets alone.
    ///
    /// Sync rather than a `tokio::test` on purpose -- `remaining` returns
    /// through `?` before it reads the clock, so an unarmed deadline needs no
    /// runtime. That is what keeps every other `bound` test above sync.
    #[test]
    fn an_unarmed_chain_deadline_changes_nothing() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.left(true, Duration::ZERO), policy.budget);
        assert_eq!(policy.left(false, Duration::ZERO), policy.connect_budget);
    }

    /// The chain's bound is the third input to the same minimum the two own
    /// bounds already take, so an armed deadline caps both of them.
    #[tokio::test(start_paused = true)]
    async fn an_armed_chain_deadline_caps_the_bound() {
        let policy = RetryPolicy::default();
        // Shorter than either own bound, so the deadline is unambiguously what
        // wins rather than coinciding with something else.
        let left = Duration::from_secs(5);
        assert!(left < policy.connect_budget);
        policy.chain_deadline.arm(left);
        assert_eq!(policy.left(true, Duration::ZERO), left);
        assert_eq!(policy.left(false, Duration::ZERO), left);

        // It is a deadline, not an allowance: spending part of it leaves the
        // rest, which is what makes the *chain* the thing being bounded.
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(policy.left(true, Duration::ZERO), Duration::from_secs(2));
    }

    /// The acceptance criterion the deadline exists for: once the chain's one
    /// budget is gone it is gone for every candidate, so candidate N gets no
    /// retries rather than a fresh `retry-budget-secs` of its own.
    #[tokio::test(start_paused = true)]
    async fn a_spent_chain_deadline_leaves_no_retries_for_the_next_candidate() {
        let policy = RetryPolicy::default();
        policy.chain_deadline.arm(Duration::from_secs(5));
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(policy.left(true, Duration::ZERO), Duration::ZERO);
        assert_eq!(
            next_delay(&policy, 0, Duration::ZERO, true, None, 1.0),
            None,
            "a candidate reached after the chain's budget is spent must not retry",
        );
    }

    /// Arming happens on the `FailoverModel`, above the candidates, and has to
    /// be visible in the retry loops underneath it. That is the whole reason
    /// the handle is an `Arc` and why `RetryPolicy` gave up `Copy` -- a clone
    /// that copied the deadline would leave every candidate unbounded.
    #[tokio::test(start_paused = true)]
    async fn a_cloned_policy_shares_the_deadline_rather_than_copying_it() {
        let policy = RetryPolicy::default();
        let candidate = policy.clone();
        policy.chain_deadline.arm(Duration::from_secs(5));
        assert_eq!(
            candidate.left(true, Duration::ZERO),
            Duration::from_secs(5),
            "arming above a candidate must be visible inside it",
        );
    }

    /// A deadline caps the own bounds; it never widens them. A chain does not
    /// buy a candidate more time than its provider was configured for.
    #[tokio::test(start_paused = true)]
    async fn a_chain_deadline_never_widens_a_candidates_own_bound() {
        let policy = RetryPolicy::default();
        policy.chain_deadline.arm(policy.budget * 2);
        assert_eq!(policy.left(true, Duration::ZERO), policy.budget);
        assert_eq!(policy.left(false, Duration::ZERO), policy.connect_budget);
    }

    /// Elapsed time counts once, not twice.
    ///
    /// `elapsed` is measured against this request's own budget; the chain
    /// deadline is an absolute instant already counting down on its own. Folding
    /// the deadline into `bound` and *then* subtracting `elapsed` charged the
    /// same seconds to both, so a chain gave up after roughly half its budget
    /// and moved off the preferred candidate early.
    #[tokio::test(start_paused = true)]
    async fn elapsed_is_not_charged_against_the_deadline_as_well() {
        let policy = RetryPolicy::default();
        policy.chain_deadline.arm(policy.budget);

        // Half the budget gone, by the clock and by the loop's own reckoning:
        // both describe the same seconds.
        let half = policy.budget / 2;
        tokio::time::advance(half).await;

        assert_eq!(
            policy.left(true, half),
            half,
            "half the budget spent must leave the other half, not nothing",
        );
        assert!(
            next_delay(&policy, 0, half, true, None, 1.0).is_some(),
            "a retry at the halfway point is still inside the budget",
        );
    }

    /// The deadline still bites when it is genuinely the shorter bound -- the
    /// property the double-subtraction fix must not undo.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_still_caps_a_candidate_that_started_late() {
        let policy = RetryPolicy::default();
        policy.chain_deadline.arm(Duration::from_secs(10));
        // The chain has burned nine of its ten seconds on an earlier candidate.
        // This one has spent nothing of its own budget, but inherits what is
        // left of the chain's.
        tokio::time::advance(Duration::from_secs(9)).await;

        assert_eq!(
            policy.left(true, Duration::ZERO),
            Duration::from_secs(1),
            "a fresh candidate gets what the chain has left, not a whole budget",
        );
    }

    /// `retry-budget-secs = 0` is the one "no retries" knob, and a chain does
    /// not give it an exception either: the minimum of zero and anything is
    /// still zero, anywhere in the chain.
    #[tokio::test(start_paused = true)]
    async fn a_zero_budget_stays_zero_under_an_armed_deadline() {
        let policy = RetryPolicy {
            budget: Duration::ZERO,
            ..RetryPolicy::default()
        };
        policy.chain_deadline.arm(Duration::from_secs(600));
        assert_eq!(policy.left(true, Duration::ZERO), Duration::ZERO);
        assert_eq!(policy.left(false, Duration::ZERO), Duration::ZERO);
        assert_eq!(
            next_delay(&policy, 0, Duration::ZERO, true, None, 1.0),
            None
        );
    }

    /// The two bounds are distinct fields, not one field that means different
    /// things depending on state -- 0002-36 reads the short one directly.
    #[test]
    fn the_two_bounds_are_separately_named_and_differently_sized() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.connect_budget, CONNECT_BUDGET);
        assert!(
            policy.connect_budget < policy.budget,
            "the connect bound must be the shorter of the two"
        );
    }

    /// The two connect constants are only useful in proportion: a connection
    /// cap at or above the budget would leave a black-holed address one try
    /// and no retry, and the "ride out a restarting gateway" case is the whole
    /// reason the budget is not zero.
    #[test]
    fn the_connect_timeout_leaves_room_for_more_than_one_try() {
        assert!(
            CONNECT_TIMEOUT * 2 <= CONNECT_BUDGET,
            "{CONNECT_TIMEOUT:?} must fit inside {CONNECT_BUDGET:?} at least twice",
        );
    }

    /// The classification the whole split rests on: a failure to get connected
    /// has to reach the loop as a *connect* failure, or `answered` latches on
    /// and buys the full budget. reqwest owns that verdict, so it is pinned
    /// rather than assumed.
    ///
    /// Refusal is the half that can be produced locally and deterministically.
    /// The other half -- a connect that times out -- has no local recipe: a
    /// stalled handshake needs SYNs to go unanswered, and the usual trick of
    /// filling a listener's accept queue does not do it, because the kernel
    /// completes the handshake from the SYN queue and the client sees a
    /// connection that is established and then silent. That is a different
    /// failure, and deliberately not this one.
    #[tokio::test]
    async fn a_refused_connection_is_a_connect_failure() {
        let error = test_client()
            .post(format!(
                "http://127.0.0.1:{REFUSED_PORT}/v1/chat/completions"
            ))
            .send()
            .await
            .expect_err("nothing listens on the discard port");
        assert!(
            error.is_connect(),
            "the loop reads `is_connect()` to mean the endpoint never answered: {error}",
        );
    }

    /// The discard port: assigned to a service essentially nothing runs, and
    /// below the ephemeral range so no test can be handed it. The crate's other
    /// unreachable-endpoint fixtures already point here.
    const REFUSED_PORT: u16 = 9;

    /// The client the socket tests drive the loop with.
    ///
    /// `no_proxy` for the same reason `remote_http_client` sets it under
    /// `cfg(test)`: reqwest reads `HTTP_PROXY` / `ALL_PROXY` automatically and
    /// exempts no address, so on a machine behind a proxy a loopback request
    /// would go to the proxy instead of being refused -- which is the one thing
    /// these tests need to happen.
    ///
    /// No `connect_timeout`, unlike the shipped client: these tests run on a
    /// paused clock, and a timer pending during real socket I/O is one the
    /// runtime may fire by auto-advancing while the connect is still in flight.
    /// Nothing here needs one -- refusal is immediate.
    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a bare client builds")
    }

    /// A policy sized for the loop tests below: the two bounds far enough apart
    /// that which one is in force is unmistakable in the elapsed time, and a
    /// backoff curve that reaches either in a handful of attempts.
    fn loop_policy() -> RetryPolicy {
        RetryPolicy {
            budget: Duration::from_secs(120),
            connect_budget: Duration::from_secs(4),
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            chain_deadline: Arc::default(),
        }
    }

    /// Drive the retry loop against `port` and report how long it spent before
    /// giving up, and what it gave up on. Time is [`tokio::time::Instant`] as
    /// the loop measures it, so under `start_paused` this is the sum of the
    /// backoffs and costs no wall clock.
    ///
    /// The label comes back because elapsed time alone cannot tell a refused
    /// connection from any other prompt failure -- the callers assert on the
    /// failure *class*, which is what makes them tests of the connect path
    /// rather than of the clock.
    async fn spend_the_budget(port: u16) -> (Duration, String) {
        let started = tokio::time::Instant::now();
        let result = send_with_retry::<Bytes>(
            test_client(),
            loop_policy(),
            Retries::default(),
            Method::POST,
            format!("http://127.0.0.1:{port}/v1/chat/completions")
                .parse()
                .expect("a loopback URI parses"),
            HeaderMap::new(),
            Bytes::from_static(b"{}"),
        )
        .await;
        let err = result.err().expect("the endpoint never succeeds");
        (started.elapsed(), failure_label(&err))
    }

    /// Confirm nothing answers on [`REFUSED_PORT`], so a test that reads a
    /// failure as "connect refused" is reading the truth.
    ///
    /// The first draft of this fixture took a port by binding to `:0` and
    /// dropping the listener. That hands the port back to the ephemeral pool,
    /// where any other test in this binary can take it before the request goes
    /// out -- and a stolen port answering `404` would satisfy a timing
    /// assertion while exercising nothing. A probe narrows that window without
    /// closing it. A port *below* the ephemeral range is never handed out by
    /// the kernel and cannot be bound without privileges, so the race is gone
    /// rather than made unlikely; the probe is left as the check that this
    /// machine is not the exception.
    async fn expect_nothing_listening() {
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", REFUSED_PORT))
                .await
                .is_err(),
            "something answers on 127.0.0.1:{REFUSED_PORT}, which these tests need closed",
        );
    }

    /// The visible bug in one test: an endpoint that refuses every connection
    /// gives up on the short bound, so a typo'd `base-url` costs seconds.
    #[tokio::test(start_paused = true)]
    async fn a_refused_endpoint_gives_up_on_the_connect_budget() {
        expect_nothing_listening().await;
        let policy = loop_policy();
        let (elapsed, label) = spend_the_budget(REFUSED_PORT).await;
        assert!(
            label.starts_with("connection error"),
            "the loop must have failed connecting, not on {label}",
        );
        assert!(
            elapsed <= policy.connect_budget,
            "a refused endpoint must give up on the connect budget, not after {elapsed:?}",
        );
    }

    /// The latch, which is the half of the split `next_delay` alone cannot
    /// show: the endpoint answers once with a `503`, then stops accepting, and
    /// every later attempt is a refused connect. Those attempts must ride the
    /// *full* budget -- a provider having a bad minute, not an address that was
    /// never right -- which is only true if `answered` stayed on.
    ///
    /// This one cannot use [`REFUSED_PORT`]: it needs a port that answers once
    /// and then refuses, which means a real listener and so an ephemeral port
    /// that goes back in the pool when it is dropped. The failure-class
    /// assertion below is what keeps a stolen port from passing as a refused
    /// connect.
    #[tokio::test(start_paused = true)]
    async fn a_503_then_a_refused_reconnect_keeps_the_full_budget() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a one-shot 503 server");
        let port = listener.local_addr().expect("loopback address").port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("one connection arrives");
            // Enough of the request to let the client finish writing; the
            // answer does not depend on it.
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\n\
                      content-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("the 503 is written");
            let _ = socket.shutdown().await;
            // Dropped before the first retry, so every later connect is
            // refused rather than queued in the accept backlog.
            drop(listener);
        });

        let policy = loop_policy();
        let (elapsed, label) = spend_the_budget(port).await;
        // The last attempt is a refused connect, so the loop rode the full
        // budget on connect failures rather than on the one `503`.
        assert!(
            label.starts_with("connection error"),
            "the retries after the 503 must have failed connecting, not on {label}",
        );
        assert!(
            elapsed > policy.connect_budget,
            "the 503 must latch the full budget on, but the loop stopped at {elapsed:?}",
        );
        assert!(
            elapsed >= policy.budget - policy.max_delay,
            "the full budget must be spent, not {elapsed:?} of it",
        );
    }

    /// A redirect loop reqwest gives up on fails the same way on every replay,
    /// so the loop returns it at once, with its budget unspent, and calls it
    /// what it is: not something a wait could clear. On the real clock: the
    /// mock is real sockets, and a paused clock would fire the budget's timers
    /// while they wait.
    #[tokio::test]
    async fn a_redirect_loop_is_final_at_once() {
        let (addr, mut requests) = super::super::mock_http::start(vec![
            super::super::mock_http::failure(307).header("Location", "/v1/messages"),
        ])
        .await;
        let policy = loop_policy();
        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            send_with_retry::<Bytes>(
                test_client(),
                policy.clone(),
                Retries::default(),
                Method::POST,
                format!("http://{addr}/v1/messages")
                    .parse()
                    .expect("a loopback URI parses"),
                HeaderMap::new(),
                Bytes::from_static(b"{}"),
            ),
        )
        .await
        .expect("a redirect loop is not retried for the whole budget")
        .err()
        .expect("a redirect loop never succeeds");

        assert!(started.elapsed() < policy.base_delay, "no wait was taken");
        let HttpError::Instance(inner) = &err else {
            panic!("a transport error: {err}")
        };
        assert!(
            inner
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_redirect),
            "{err}"
        );
        assert!(!is_recoverable(&CompletionError::HttpError(err)));
        let followed = super::super::mock_http::drain(&mut requests).len();
        assert!(
            (2..=11).contains(&followed),
            "one request and the redirects reqwest follows, once: {followed}"
        );
    }

    /// What a retry is recorded and announced as names the status, never the
    /// provider's body.
    #[test]
    fn the_failure_label_names_the_status_without_the_body() {
        let CompletionError::HttpError(err) = http_status(429) else {
            unreachable!("an HTTP failure")
        };
        let label = failure_label(&err);
        assert!(label.contains("429"), "{label}");
        assert!(!label.contains("boom"), "the body must not leak: {label}");
    }
}
