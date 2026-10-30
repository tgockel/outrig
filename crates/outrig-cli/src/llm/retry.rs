//! Transient-error retry, in two layers.
//!
//! [`RetryingHttpClient`] handles failures the transport can see -- a status
//! code or a dead connection. [`RetryingModel`] handles the one it cannot: a
//! `200 OK` whose body carries no usable content, which is a success to the
//! transport and a [`ProviderError::Response`] to rig.
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
//! Retrying transport failures *there* rather than around a model call is what
//! keeps a failed request one request: by the time a failure has become a
//! [`ProviderError`] rig has classified it for its own retrying, which the
//! REPL never does, and the REPL reads only which class it was.
//!
//! [`RetryingModel`] wraps the model itself and covers what is left: a response
//! rig could not turn into a completion, or made one of that no model ended --
//! a reply cut off with nothing in it, or ended by the provider for its content.
//! It takes the same budget, and an attempt count on top, because an unusable
//! body comes back immediately -- the budget alone would spend itself on dozens
//! of tries instead of the handful a hiccup deserves. Sharing the budget keeps
//! `retry-budget-secs = 0` the one "no retries" knob. It does not retry an empty
//! reply the model ended of its own accord ([`unusable_reply`]).
//!
//! The obvious "just use the ecosystem crate" answer is `reqwest-retry`, which
//! rig itself carries as a dev-dependency. It does not fit: as of 0.9 it does
//! not read `Retry-After` at all, which is the feature this exists for.
//!
//! What a failed turn was is read back at the REPL from the report the chain
//! leaves rig with ([`super::failover::report`]): the two classes this module
//! retries each have a code ([`CODE_TRANSIENT`], [`CODE_UNUSABLE`]), and the
//! predicates at the end read them.

// rig's `ProviderError` and `http_client::Error` are what its model and
// transport traits hand back by value, and what this module carries between
// them; boxing them in OutRig's own functions would only move the size.
#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rand::RngExt;
use reqwest::StatusCode;
use rig::DynModel;
use rig::completion::{
    AssistantContent, CompletionRequest, CompletionResponse, FinishReason, PromptError,
    ProviderCapabilities,
};
use rig::error::{ErrorReport, ProviderError};
use rig::http_client::{
    Error as HttpError, HeaderMap, HttpClientExt, LazyBody, Method, MultipartForm, ReqwestClient,
    Request, Response, StreamingResponse, Uri,
};
use rig::operation::Completion;
use rig::wasm_compat::WasmCompatSend;

use super::is_blank;

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

/// The code a failed call's report carries when one endpoint stayed
/// transiently broken -- rate-limited, unreachable -- for as long as retries
/// allowed; its message is the failure's label.
pub(crate) const CODE_TRANSIENT: &str = "outrig.transient";
/// The code a failed call's report carries when the provider's response could
/// not be used, after [`RetryingModel`] spent its attempts on it; its message
/// is the provider-side detail.
pub(crate) const CODE_UNUSABLE: &str = "outrig.unusable";

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
pub struct ChainDeadline {
    /// `None` until a chain arms it. Every agent's model is a chain, of one
    /// model or more, so only a policy outside one -- tests -- stays unarmed,
    /// with the per-attempt budgets its only bound.
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
/// Not `Copy`, which it was until failover arrived: the whole policy used to
/// move into a `'static` per-request future without an `Arc`, and it now
/// carries one. That `Arc` is [`chain_deadline`], the bound a failover chain
/// shares across its candidates -- a handle rather than a value precisely
/// because the arming happens outside the candidate that must observe it.
/// Cloning stays cheap (four `Duration`s and a refcount bump), and a clone
/// deliberately *shares* the deadline rather than copying it, which is the
/// whole point of the indirection.
///
/// [`chain_deadline`]: Self::chain_deadline
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Wall clock from the first attempt's start, including the time each
    /// attempt spends in flight -- not only the time spent sleeping. Zero
    /// disables retries.
    pub budget: Duration,
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
    pub connect_budget: Duration,
    /// First backoff delay, doubling each attempt.
    pub base_delay: Duration,
    /// Ceiling on one backoff delay. Does not cap a server's `Retry-After`.
    pub max_delay: Duration,
    /// The failover chain's shared bound, when this policy belongs to one.
    ///
    /// It costs an uncontended lock and an `Option` check per delay decision --
    /// on a path that then sleeps for at least `MIN_DELAY`. See
    /// [`ChainDeadline`].
    pub chain_deadline: Arc<ChainDeadline>,
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
            budget: Duration::from_secs(outrig::config::DEFAULT_RETRY_BUDGET_SECS),
            connect_budget: CONNECT_BUDGET,
            base_delay: BASE_DELAY,
            max_delay: MAX_DELAY,
            chain_deadline: Arc::default(),
        }
    }
}

/// A `reqwest::Client` that retries transient failures, handed to rig in place
/// of its own transport. See the module doc for why the retry lives at this
/// layer.
///
/// `Default` is for tests: it yields the shipped policy against a default
/// `reqwest::Client`, which notably has *no* request timeout; `build_agent`
/// always supplies a configured client, which sets one.
#[derive(Clone, Debug, Default)]
pub struct RetryingHttpClient {
    inner: reqwest::Client,
    /// The same client as rig's own transport, for the requests this one does
    /// not retry.
    transport: ReqwestClient,
    policy: RetryPolicy,
}

impl RetryingHttpClient {
    pub fn new(inner: reqwest::Client, policy: RetryPolicy) -> Self {
        Self {
            transport: ReqwestClient::from(inner.clone()),
            inner,
            policy,
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
        self.transport.send_multipart(req)
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
        self.transport.send_streaming(req)
    }
}

/// A model that retries a response rig could not use, which a chain's
/// candidate holds in place of the provider's own model.
///
/// The failure it exists for is a provider answering `200 OK` with nothing a
/// turn can go on with: a body rig could not decode, or one it decoded to a
/// reply no model ended -- cut off with nothing in it, or ended by the
/// provider for its content ([`unusable_reply`]). It used to end the session
/// six turns into a conversation. [`RetryingHttpClient`] cannot see it: a
/// `200` is a success down there.
///
/// Every such reply becomes a [`ProviderError::Response`], rig's own class for
/// "the provider's body could not be turned into a completion", which is
/// always the provider's doing and never a local misconfiguration; the loop
/// retries that class. A reply with nothing in it that the provider says the
/// model ended on its own is no defect: rig makes an empty turn of it, which
/// its run loop reads as a turn that finished, and it passes through. A reply
/// with anything in it, a refusal for one, is still an error.
///
/// A model-layer wrapper is what `deedd83` deliberately removed when the
/// transient retry moved down to [`RetryingHttpClient`], and it brings back the
/// per-call `CompletionRequest` clone that commit was glad to lose. It earns it
/// by catching a class that layer cannot see at all, and the clone is skipped
/// entirely when retries are off. Against the JSON serialization of that same
/// request on the very next line, one clone is the cheaper half.
#[derive(Clone, Debug)]
pub struct RetryingModel {
    inner: DynModel<Completion>,
    policy: RetryPolicy,
}

impl RetryingModel {
    pub fn new(inner: impl Into<DynModel<Completion>>, policy: RetryPolicy) -> Self {
        Self {
            inner: inner.into(),
            policy,
        }
    }

    /// What the provider composes, as rig's wire for it reports.
    pub(crate) fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities().completion
    }

    /// One completion, retried while the reply is unusable.
    pub async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        // Read off the request before rig consumes it: naming the ceiling that
        // was in force is most of what makes a truncation report actionable,
        // and `None` -- no ceiling sent, provider's default silently in charge
        // -- is the case worth naming loudest.
        let max_tokens = request.max_tokens;
        let outcome = self.completion_retried(request).await;
        if let Ok(response) = &outcome {
            report_textless_completion(response, max_tokens);
        }
        outcome
    }

    /// One completion from the inner model. A reply rig made a completion of
    /// that no model ended ([`unusable_reply`]) is the error it comes back as,
    /// and so is a body rig rejected ([`undecodable`]), so the loop above has
    /// one class to retry.
    async fn attempt(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        match self.inner.call(request).await {
            Ok(response) => match unusable_reply(&response) {
                Some(message) => Err(ProviderError::Response(message)),
                None => Ok(response),
            },
            Err(error) => Err(match undecodable(error) {
                Ok(message) => ProviderError::Response(message),
                Err(error) => error,
            }),
        }
    }

    /// The retry loop proper. Split out so [`Self::completion`] can inspect
    /// what came back without the reporting having to live inside the loop and
    /// fire once per attempt.
    async fn completion_retried(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
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
            let message = match self.attempt(request.clone()).await {
                Err(ProviderError::Response(message)) => message,
                // A success, or a failure this layer cannot fix: one the HTTP
                // client already retried, or a terminal one -- a bad key's
                // `401` above all, which has to keep ending the process.
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
                    next_delay(&self.policy, attempt, started.elapsed(), true, None, jitter())
                })
                .flatten();
            let Some(delay) = delay else {
                return Err(ProviderError::Response(message));
            };

            eprintln!(
                "[outrig] model returned an unusable response ({message}); retry in {:.1}s \
                 (attempt {}/{})",
                delay.as_secs_f64(),
                attempt + 2,
                RESPONSE_RETRY_ATTEMPTS + 1,
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// Why a completion rig made is no reply a turn can go on with, when it is
/// none: a reply the provider ended for its content ([`is_content_refusal`]),
/// one the provider itself says failed, or one cut off -- at the output
/// ceiling, or for a reason that is no stop -- with nothing in it. `None` for
/// a reply with anything in it, and for an empty one the model ended on its
/// own, which rig's run loop reads as a turn that finished.
///
/// A reply the provider ended for its content reads as a turn the model
/// finished, whatever came before the cut, so it is an error as an empty reply
/// is, and the retry re-rolls it.
///
/// The finish reason is the provider's own word for it, read from the reply's
/// document ([`provider_finish_reason`]), so the error names what the provider
/// said; rig's normalized reason stands in when the document has none.
fn unusable_reply(response: &CompletionResponse) -> Option<String> {
    let word = provider_finish_reason(&response.raw);
    let normalized = response.finish_reason();
    let refusal = word.as_deref().is_some_and(is_content_refusal)
        || matches!(normalized, Some(FinishReason::ContentFilter));
    let reason = word.or_else(|| normalized.clone().map(finish_word));
    if refusal {
        let reason = reason.unwrap_or_else(|| "content_filter".to_string());
        return Some(format!(
            "finish reason {reason}: the provider ended the reply for its content"
        ));
    }
    if let Some(error) = &response.error {
        return Some(format!("the provider failed the turn: {error}"));
    }
    // Blank text is nothing; reasoning is something, which rig's run loop
    // reads for itself: a reply of only reasoning that was cut off is a turn
    // it fails, not one to re-roll.
    let empty = response.choice.iter().all(|part| match part {
        AssistantContent::Text(text) => is_blank(&text.text),
        _ => false,
    });
    let normal = match &normalized {
        None | Some(FinishReason::Stop) | Some(FinishReason::ToolCalls) => true,
        Some(FinishReason::Other(word)) => is_normal_stop(word),
        _ => false,
    };
    (empty && !normal).then(|| {
        let reason = reason.unwrap_or_else(|| "unknown".to_string());
        format!("finish reason {reason}: the reply holds no text and no tool call")
    })
}

/// The wire's spelling of a normalized finish reason, for an error that names
/// one when the reply's document does not.
fn finish_word(reason: FinishReason) -> String {
    match reason {
        FinishReason::Stop => "stop".to_string(),
        FinishReason::Length => "length".to_string(),
        FinishReason::ToolCalls => "tool_calls".to_string(),
        FinishReason::ContentFilter => "content_filter".to_string(),
        FinishReason::Other(word) => word,
        _ => "unknown".to_string(),
    }
}

/// `error` as the message of a `200` rig could make nothing of, when it is
/// one: a body it rejected, one that ended before the provider ended it, or a
/// provider's error envelope under a success status. Any other failure is
/// handed back as it is: the HTTP layer already retried it, or no retry can
/// clear it -- a body that is not JSON among them, which a later turn may get
/// past but a resend of the same request will not.
fn undecodable(error: ProviderError) -> Result<String, ProviderError> {
    match error {
        ProviderError::Response(message) => Ok(message),
        ProviderError::Truncated => Ok("the reply ended before the provider ended it".to_string()),
        error if error
            .provider_response_status()
            .is_some_and(|status| status.is_success()) =>
        {
            Ok(error.to_string())
        }
        error => Err(error),
    }
}

/// Whether `reason`, a provider's finish reason, says the model chose to stop:
/// OpenAI's `stop` and Anthropic's `end_turn`. Every other reason of an empty
/// reply -- a truncation, a filter -- is a turn that was cut short, and the
/// provider's defect to retry.
fn is_normal_stop(reason: &str) -> bool {
    matches!(reason, "stop" | "end_turn")
}

/// Whether `reason`, a provider's finish reason, says the provider would not
/// answer for what the request or the reply held: OpenAI's `content_filter`
/// and Anthropic's `refusal`. A reply ending so is unusable, whatever it holds.
fn is_content_refusal(reason: &str) -> bool {
    matches!(reason, "content_filter" | "refusal")
}

/// Report a completion that came back with nothing to show for itself.
///
/// A response carrying no text and no tool call is a turn the user paid for and
/// cannot see. Rig treats it as an ordinary success -- `output` is the text
/// parts concatenated, so a reasoning-only turn is simply the empty string --
/// and every layer above the model has already lost the provider's own account
/// of what happened. The model layer is the last point that still holds it, so
/// that is where this runs, in the words [`textless_line`] gives.
///
/// Deliberately keyed on the *decoded* content rather than on the finish
/// reason: that check is free on every turn, and the raw response is only
/// read in the rare case that already went wrong.
fn report_textless_completion(response: &CompletionResponse, max_tokens: Option<u64>) {
    if showed_something(&response.choice) {
        return;
    }

    let finish_reason = provider_finish_reason(&response.raw);
    eprintln!("{}", textless_line(finish_reason.as_deref(), max_tokens));
}

/// Whether a completion showed the user anything: non-blank text, or a tool
/// call. Reasoning is not shown.
///
/// An empty text part is not content: it is what an empty provider turn
/// decodes to, so treating it as "the model spoke" is exactly the mistake
/// [`report_textless_completion`] exists to catch. Blankness is [`is_blank`]'s,
/// the question `TurnEnd::is_silent` asks of the reply.
fn showed_something(choice: &[AssistantContent]) -> bool {
    choice.iter().any(|part| match part {
        AssistantContent::Text(text) => !is_blank(&text.text),
        AssistantContent::ToolCall(_) => true,
        _ => false,
    })
}

/// The line printed for a model call that showed the user nothing.
fn textless_line(finish_reason: Option<&str>, max_tokens: Option<u64>) -> String {
    let reason = finish_reason
        .map(|reason| format!(" (provider finish reason: {reason:?})"))
        .unwrap_or_default();
    let ceiling = match max_tokens {
        Some(limit) => format!("the request carried max-tokens = {limit}"),
        None => "the request carried no max-tokens, so the provider's own default applied".into(),
    };
    format!("[outrig] the model produced no text and no tool call this turn{reason}; {ceiling}.")
}

/// The provider's own word for why generation stopped, from the completion's
/// document: the provider's reply as rig received it, which it keeps beside
/// the normalized reason. Both dialects are checked because both are
/// reachable: `choices[].finish_reason` is the OpenAI shape and `stop_reason`
/// the Anthropic one. Returns `None` for a provider that names neither, which
/// costs the report one clause and nothing else.
fn provider_finish_reason(raw: &serde_json::Value) -> Option<String> {
    let word = raw
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("finish_reason"))
        .or_else(|| raw.get("stop_reason"))?
        .as_str()?;
    Some(word.to_string())
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
    // Whether any attempt has got bytes back from the endpoint. Latches on and
    // never clears: once the endpoint has answered, a later failure to
    // reconnect is a provider having a bad minute rather than an address that
    // was never right, so the full budget applies for the rest of the request.
    let mut answered = false;
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
                answered = true;
                let status = response.status();
                // Read before `text()` consumes the response.
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| parse_retry_after(value, jiff::Timestamp::now()));
                // Mirrors rig-reqwest's own `non_success_status_error`, so a
                // failure that escapes this loop is shaped exactly as it would
                // have been without the wrapper -- status, headers and body --
                // which is what keeps rig's `provider_response_*` readers
                // working.
                //
                // Reading the body to completion also matters on the *retry*
                // path, where the message is dropped unused: abandoning a
                // partially-read body makes hyper drop the connection instead
                // of returning it to the pool, so the retry would pay a fresh
                // TCP and TLS handshake to save a few KiB of read.
                let response_headers = response.headers().clone();
                let message = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
                let err = HttpError::non_success_with_details(status, response_headers, message);
                if !is_retryable_status(status) {
                    return Err(err);
                }
                (err, retry_after)
            }
            // A request that cannot succeed as sent fails the same way however
            // often it is sent, so it is final at once rather than spending the
            // budget -- and with it a failover chain's -- on replays.
            Err(error) if is_permanent(&error) => {
                return Err(HttpError::instance(error));
            }
            // Read timeouts, connection resets, and the like -- all
            // retry-worthy, and none of them carry a `Retry-After`.
            Err(error) => {
                // Read here because the box below erases the concrete type, so
                // this is the only place a connect failure is still knowable.
                answered |= !error.is_connect();
                (HttpError::instance(error), None)
            }
        };

        let elapsed = started.elapsed();
        let Some(delay) = next_delay(&policy, attempt, elapsed, answered, retry_after, jitter())
        else {
            return Err(err);
        };
        // Kept under 100 columns for a realistic status line and budget, so a
        // wait does not wrap in a terminal the user is watching. The budget
        // shown is the one actually being spent against, so a connect failure
        // does not count down against a ten-minute bound it will never reach.
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
            policy.bound(answered).as_secs(),
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// Repackage a successful `reqwest::Response` as the `http::Response` rig
/// expects, with the body left lazy. Mirrors rig-reqwest's `into_response`,
/// less the binding of the body to rig's own runtime, which a process on
/// tokio does not need. rig exposes no public converter for this, so it is a
/// copy by necessity rather than by choice.
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
    let body: LazyBody<U> = Box::pin(async move {
        let bytes = response.bytes().await.map_err(HttpError::instance)?;
        Ok(U::from(bytes))
    });
    res.body(body).map_err(HttpError::Protocol)
}

/// The retry-worthy statuses: request timeouts, "too early", rate limits, and
/// the server-side 5xx family. Everything else -- other 4xx especially -- is
/// terminal, because replaying it would fail the same way.
///
/// The 5xx family is taken whole because providers put nonstandard codes in
/// it -- Anthropic's `529` overloaded, Cloudflare's `520`-`527` -- that an
/// explicit list of the standard ones misses.
fn is_retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429)
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

/// A short label for a status: the status line, never the provider's error
/// body -- a rate-limited gateway's body runs to kilobytes of nested JSON.
///
/// A code with no standard reason phrase is shown bare: `StatusCode`'s own
/// `Display` would print `529 <unknown status code>`, which says nothing more
/// and pushes the retry line past a terminal's width.
fn status_label(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(_) => format!("HTTP {status}"),
        None => format!("HTTP {}", status.as_u16()),
    }
}

/// A short label for a failure: the status line, or the transport error.
fn failure_label(err: &HttpError) -> String {
    match err {
        HttpError::InvalidStatusCodeWithDetails { status, .. } => status_label(*status),
        HttpError::Instance(inner) => {
            format!("connection error: {}", with_innermost_cause(inner.as_ref()))
        }
        other => other.to_string(),
    }
}

/// An error's `Display` followed by the innermost link of its `source()` chain.
///
/// reqwest's own text is the kind of failure and the URL; whether the connect
/// was refused, the lookup failed, or TLS did not match is only in the chain.
/// One link rather than all of them, since this is a status line. Left alone
/// when there is no source or the outer text already says it.
fn with_innermost_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let outer = err.to_string();
    let mut innermost = None;
    let mut link = err.source();
    while let Some(cause) = link {
        innermost = Some(cause);
        link = cause.source();
    }
    match innermost.map(ToString::to_string) {
        Some(cause) if !outer.contains(&cause) => format!("{outer}: {cause}"),
        _ => outer,
    }
}

/// [`failure_label`] for a transport failure, which is the one kind of failure
/// it words better than rig's `Display` does: that wraps the same outer text
/// behind `HttpError:` and drops the cause. `None` for anything else.
fn transport_label(err: &ProviderError) -> Option<String> {
    match err {
        ProviderError::Http(http) if matches!(&**http, HttpError::Instance(_)) => {
            Some(failure_label(http))
        }
        _ => None,
    }
}

/// The label for a failed candidate in a failover chain's move line and
/// `tried` rows: a transport failure reads as it does on a retry line, and
/// anything else keeps rig's own text.
pub(crate) fn candidate_failure_label(err: &ProviderError) -> String {
    transport_label(err).unwrap_or_else(|| err.to_string())
}

/// Classify an HTTP failure as transient -- the same predicate the retry loop
/// applies, so what the REPL calls recoverable is exactly what was retried.
fn is_transient(err: &HttpError) -> bool {
    match err {
        HttpError::InvalidStatusCodeWithDetails { status, .. } => is_retryable_status(*status),
        HttpError::Instance(inner) => !inner
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_permanent),
        _ => false,
    }
}

/// The label of a failure that stayed transient for as long as retries
/// allowed, when `err` is one: a transport failure, or a status the HTTP loop
/// retried until its budget stopped it -- which rig hands on as the provider's
/// response, its status kept. `None` for anything else.
///
/// Sound only because everything that can retry such a failure *did* retry it
/// until the budget stopped it. An early return for a retryable status in
/// [`send_with_retry`], or a second layer that retried this class, would make
/// this claim more than it knows. Note it stays true when the budget is `0`:
/// nothing was retried, and there was nothing to retry with.
///
/// Note the deliberate reach: a `base-url` naming the wrong host fails with a
/// connection error, which is transient by this predicate, so an unreachable
/// endpoint ends the turn rather than the session. That is right for a REPL --
/// the message names the connection failure and the user can fix the config or
/// `/quit` -- and the *wait* before it no longer follows the full budget: an
/// endpoint that never answered is bounded by [`CONNECT_BUDGET`], so a typo
/// costs seconds rather than minutes. The classification is the decision; the
/// wait was the bug.
///
/// The reach stops short of a request reqwest refuses to send at all: a scheme
/// other than `http` or `https`, a URL it cannot parse, a redirect loop that
/// reached its limit. [`send_with_retry`] returns that at once, and it is
/// terminal here for the reason a `401` is -- no resend can satisfy it. A
/// config cannot carry the scheme typo that was the usual way in: a load
/// refuses it.
pub(crate) fn transient_label(err: &ProviderError) -> Option<String> {
    match err {
        ProviderError::Http(http) if is_transient(http) => Some(failure_label(http)),
        other => other
            .provider_response_status()
            .filter(|status| is_retryable_status(*status))
            .map(status_label),
    }
}

/// Did this turn fail transiently -- and so recoverably -- rather than for a
/// reason a second attempt could not fix? Returns the label to report if so.
///
/// Every agent's model is a chain, so a failure reaches the REPL as the report
/// the chain left rig with ([`super::failover::report`]), which names this
/// class by [`CODE_TRANSIENT`]; a provider error rig hands on bare is read
/// directly ([`transient_label`]). A multi-candidate chain's exhaustion is a
/// class of its own, [`chain_exhausted_label`], so an error reaching here as
/// this class still means what it always did -- *the* endpoint this turn had
/// stayed broken for its whole budget.
///
/// Returning the label rather than a `bool` keeps the classification and the
/// thing to print together: a caller cannot decide this is recoverable without
/// also holding the reason to show.
///
/// [`chain_exhausted_label`]: super::failover::chain_exhausted_label
pub fn exhausted_transient_label(err: &PromptError) -> Option<String> {
    match err {
        PromptError::Report(report) if report.code.as_deref() == Some(CODE_TRANSIENT) => {
            Some(report.message.clone())
        }
        PromptError::Provider(failed) => transient_label(failed),
        _ => None,
    }
}

/// Would this failure, on its own, end the *turn* rather than the process?
///
/// The union of the two recoverable classes, asked of a bare [`ProviderError`]
/// rather than a [`PromptError`]: a transient failure, which
/// [`exhausted_transient_label`] claims, and an unusable reply, which
/// [`unusable_response_label`] does. Everything else -- a `401`, a model that
/// does not exist, a provider-reported fault -- is terminal, and telling a user
/// to resend a prompt their config can never satisfy would loop them forever.
///
/// Exists because a failover chain has to ask this of each candidate's error
/// *before* aggregating them. Defined here, beside the loops whose classes it
/// unions, so a chain's verdict on an error cannot drift from what was
/// retried.
pub(crate) fn is_recoverable(err: &ProviderError) -> bool {
    match err {
        ProviderError::Http(http) => is_transient(http),
        ProviderError::Response(_) | ProviderError::Truncated => true,
        other => other
            .provider_response_status()
            .is_some_and(is_retryable_status),
    }
}

/// Did this turn fail because the provider's response could not be used, after
/// [`RetryingModel`] spent its attempts on it? Returns the provider-side detail
/// to report if so.
///
/// The companion to [`exhausted_transient_label`], and sound for the same
/// reason: there is exactly one layer that retries this class, and it retried
/// until its attempts ran out. A chain does not become a second one -- it
/// aggregates rather than re-runs, so this class arriving here came from a
/// single candidate that already spent its attempts.
///
/// Deliberately *not* extended to [`ProviderError::Provider`], which carries
/// provider-reported faults that include genuine misconfiguration -- telling a
/// user to resend a prompt their config can never satisfy would loop them
/// forever.
pub fn unusable_response_label(err: &PromptError) -> Option<&str> {
    match err {
        PromptError::Report(report) if report.code.as_deref() == Some(CODE_UNUSABLE) => {
            Some(&report.message)
        }
        PromptError::Provider(ProviderError::Response(message)) => Some(message),
        _ => None,
    }
}

/// The report a lone candidate's failure leaves rig with: its class as a code
/// where the REPL tells classes apart, and a label fit to print as its
/// message. A chain of more than one reports its exhaustion instead
/// ([`super::failover::report`]).
pub(crate) fn report(error: &ProviderError) -> ErrorReport {
    let mut report = ErrorReport::from(error);
    if let Some(label) = transient_label(error) {
        report.message = label;
        report.code = Some(CODE_TRANSIENT.to_string());
    } else if let ProviderError::Response(detail) = error {
        report.message = detail.clone();
        report.code = Some(CODE_UNUSABLE.to_string());
    } else {
        report.message = candidate_failure_label(error);
    }
    report
}

#[cfg(test)]
mod tests {
    use rig::completion::Message;
    use rig::completion::message::{Origin, ToolName};
    use rig::driver::{Exchange, Model, Opened, Opening, Transport};
    use rig::test_utils::{MOCK_API, MOCK_PROVIDER, MockFrame, MockScript};

    use super::*;

    /// A non-success status, as rig classifies one the HTTP loop handed back:
    /// the provider's response, its status kept.
    fn http_status(code: u16) -> ProviderError {
        ProviderError::from_http_response(StatusCode::from_u16(code).unwrap(), "boom")
    }

    /// A transport failure, as rig wraps one the HTTP loop handed back.
    fn transport(error: impl std::error::Error + Send + Sync + 'static) -> ProviderError {
        ProviderError::Http(Arc::new(HttpError::instance(error)))
    }

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().expect("fixed timestamp")
    }

    /// What the REPL sees for a given completion failure of a lone candidate:
    /// the report the chain leaves rig with, as `handle_prompt_error` reads it.
    fn label_of(err: ProviderError) -> Option<String> {
        exhausted_transient_label(&PromptError::Report(report(&err)))
    }

    /// The only 4xx statuses that retry; the tables below split 4xx on it.
    const RETRYABLE_4XX: [u16; 3] = [408, 425, 429];

    #[test]
    fn retryable_status_codes_are_transient() {
        for code in RETRYABLE_4XX.into_iter().chain(500..=599) {
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
        for code in (400..=499).filter(|code| !RETRYABLE_4XX.contains(code)) {
            let status = StatusCode::from_u16(code).unwrap();
            assert!(!is_retryable_status(status), "{code} should not retry");
            assert!(
                label_of(http_status(code)).is_none(),
                "{code} should stay fatal at the REPL",
            );
        }
    }

    #[test]
    fn a_nonstandard_status_is_labeled_by_its_code() {
        assert_eq!(label_of(http_status(529)).as_deref(), Some("HTTP 529"));
        assert_eq!(
            label_of(http_status(503)).as_deref(),
            Some("HTTP 503 Service Unavailable"),
        );
    }

    #[test]
    fn transport_errors_are_transient() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out");
        assert!(label_of(transport(io)).is_some());
    }

    /// A provider error rig hands on bare -- not through a chain's report --
    /// is classified the same way.
    #[test]
    fn a_bare_provider_error_is_classified_too() {
        assert_eq!(
            exhausted_transient_label(&PromptError::Provider(http_status(503))).as_deref(),
            Some("HTTP 503 Service Unavailable"),
        );
        assert_eq!(
            unusable_response_label(&PromptError::Provider(ProviderError::Response(
                "empty".into()
            ))),
            Some("empty"),
        );
    }

    /// The two recoverable classes partition what they recognize -- an HTTP
    /// failure is not an unusable response and vice versa -- and everything
    /// they both decline stays terminal, which is what keeps a bad key an
    /// exit-1 rather than an invitation to resend forever.
    #[test]
    fn the_recoverable_classes_are_disjoint_and_do_not_cover_everything() {
        let unusable_label_of = |err: ProviderError| {
            unusable_response_label(&PromptError::Report(report(&err))).map(str::to_owned)
        };

        assert_eq!(
            unusable_label_of(ProviderError::Response("empty".into())),
            Some("empty".to_string()),
        );
        assert!(label_of(ProviderError::Response("empty".into())).is_none());

        assert!(unusable_label_of(http_status(503)).is_none());
        assert!(label_of(http_status(503)).is_some());

        // Neither class claims a provider-reported fault: it may well be a
        // misconfiguration no retry and no resend can fix.
        assert!(label_of(ProviderError::Provider("nope".into())).is_none());
        assert!(unusable_label_of(ProviderError::Provider("nope".into())).is_none());
    }

    /// Failover is a third class, and the three stay disjoint by the code a
    /// report carries rather than by the order they are consulted.
    ///
    /// This is what `exhausted_transient_label`'s soundness rests on: a chain
    /// aggregates into a report of its own class, so no candidate's transient
    /// failure reaches the transient predicate carrying "one endpoint failed"
    /// when the truth is "every candidate did".
    #[test]
    fn a_chain_exhaustion_is_a_class_of_its_own() {
        use super::super::failover::chain_exhausted_label;

        let chain_label_of = |err: ProviderError| {
            chain_exhausted_label(&PromptError::Report(report(&err)))
                .map(|chain| chain.tried.to_owned())
        };

        // The classes the other two claim are not claimed by this one.
        assert!(chain_label_of(http_status(503)).is_none());
        assert!(chain_label_of(ProviderError::Response("empty".into())).is_none());
        // Nor is a provider fault that is not a chain's report: it still
        // belongs to none of the three and stays terminal, which is what
        // keeps a bad key an exit-1.
        assert!(chain_label_of(ProviderError::Provider("nope".into())).is_none());
    }

    /// A model that answers every request `200` with a reply nothing can be
    /// made of -- cut off at its ceiling with nothing in it -- and counts
    /// them.
    #[derive(Clone)]
    struct AlwaysUnusable(Arc<std::sync::atomic::AtomicUsize>);

    impl Transport<MockScript> for AlwaysUnusable {
        fn send(&self, _request: CompletionRequest, _exchange: Exchange) -> Opening<MockFrame> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let empty = CompletionResponse::new(
                Vec::new(),
                rig::completion::Usage::new(),
                Origin::new(MOCK_API, MOCK_PROVIDER, ""),
                serde_json::Value::Null,
            )
            .with_optional_finish_reason(Some(FinishReason::Length));
            Opening::ready(Opened::new(futures_util::stream::iter([Ok(
                MockFrame::Response(Box::new(empty)),
            )])))
        }
    }

    /// [`AlwaysUnusable`] as the model a [`RetryingModel`] wraps.
    fn always_unusable(calls: &Arc<std::sync::atomic::AtomicUsize>) -> DynModel<Completion> {
        Model::new(MockScript::default(), AlwaysUnusable(Arc::clone(calls))).erase()
    }

    /// The count is what has to stop a provider answering empty forever: an
    /// unusable response comes back in milliseconds, so on the default
    /// ten-minute budget the backoff curve alone would keep re-rolling for the
    /// whole of it with the user watching. `start_paused` means the waits cost
    /// this test no wall clock.
    #[tokio::test(start_paused = true)]
    async fn an_endlessly_unusable_response_gives_up_after_a_bounded_number_of_tries() {
        let calls: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let model = RetryingModel::new(always_unusable(&calls), RetryPolicy::default());

        let err = model
            .completion(CompletionRequest::new("hi"))
            .await
            .expect_err("a model that only ever answers empty never recovers");

        assert!(matches!(err, ProviderError::Response(_)), "{err}");
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
        let calls: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let model = RetryingModel::new(
            always_unusable(&calls),
            RetryPolicy {
                budget: Duration::ZERO,
                ..RetryPolicy::default()
            },
        );

        model
            .completion(CompletionRequest::new("hi"))
            .await
            .expect_err("the first failure is final");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
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
            next_delay(&policy, 0, policy.budget + Duration::from_secs(1), true, None, 1.0),
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
            next_delay(&policy, 0, elapsed, true, Some(Duration::from_secs(30)), 1.0),
            None,
        );
    }

    #[test]
    fn a_zero_budget_disables_retries() {
        let policy = RetryPolicy {
            budget: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_eq!(next_delay(&policy, 0, Duration::ZERO, true, None, 1.0), None);
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
        assert_eq!(next_delay(&policy, 0, Duration::ZERO, false, None, 1.0), None);
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

    /// Unarmed is a policy outside any chain, and it must cost nothing: the
    /// two bounds are exactly what they were before failover existed.
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
        assert_eq!(next_delay(&policy, 0, Duration::ZERO, true, None, 1.0), None);
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
            .post(format!("http://127.0.0.1:{REFUSED_PORT}/v1/chat/completions"))
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

    /// The innermost `source()` text of a refused connect to [`REFUSED_PORT`],
    /// taken from the error itself because the OS wording varies.
    async fn refused_cause() -> String {
        let err = test_client()
            .get(format!("http://127.0.0.1:{REFUSED_PORT}/"))
            .send()
            .await
            .expect_err("nothing listens");
        let mut cause: &(dyn std::error::Error + 'static) = &err;
        while let Some(next) = cause.source() {
            cause = next;
        }
        cause.to_string()
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
            label.ends_with(&refused_cause().await),
            "the label must carry the connect failure's own cause: {label}",
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

    /// A URL reqwest refuses to send fails the same way on every replay, so the
    /// loop returns it at once, with its budget unspent, and calls it what it
    /// is: not something a wait or a resend could clear. The URL has a typo'd
    /// scheme (#343). reqwest refuses it before it opens a socket, so the
    /// paused clock counts only the waits the loop took.
    #[tokio::test(start_paused = true)]
    async fn a_url_reqwest_refuses_is_final_at_once() {
        let started = tokio::time::Instant::now();
        let err = send_with_retry::<Bytes>(
            test_client(),
            loop_policy(),
            Method::POST,
            format!("htps://127.0.0.1:{REFUSED_PORT}/v1/chat/completions")
                .parse()
                .expect("http parses any scheme"),
            HeaderMap::new(),
            Bytes::from_static(b"{}"),
        )
        .await
        .err()
        .expect("reqwest refuses an htps:// URL");

        assert_eq!(started.elapsed(), Duration::ZERO, "no wait was taken");
        let HttpError::Instance(inner) = &err else {
            panic!("a transport error: {err}")
        };
        assert!(
            inner
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_builder),
            "{err}"
        );
        let err = ProviderError::Http(Arc::new(err));
        assert!(
            !is_recoverable(&err),
            "a chain must not call it recoverable"
        );
        assert!(
            label_of(err).is_none(),
            "the REPL must not call it transient"
        );
    }

    /// A chain of `depth` links: each one's `Display` is its own name.
    #[derive(Debug)]
    struct Link(&'static str, Option<Box<Link>>);

    impl std::fmt::Display for Link {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Link {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|l| l as _)
        }
    }

    #[test]
    fn the_innermost_cause_is_appended_once() {
        let err = Link("outer", Some(Box::new(Link("middle", Some(Box::new(Link("root", None)))))));
        assert_eq!(with_innermost_cause(&err), "outer: root");
    }

    #[test]
    fn a_cause_the_outer_message_already_has_is_not_repeated() {
        let err = Link("failed: root", Some(Box::new(Link("root", None))));
        assert_eq!(with_innermost_cause(&err), "failed: root");
    }

    #[test]
    fn an_error_with_no_source_renders_as_it_always_did() {
        assert_eq!(with_innermost_cause(&Link("alone", None)), "alone");
    }

    #[test]
    fn a_candidate_transport_failure_carries_the_cause_without_rigs_prefix() {
        let err = transport(Link(
            "error sending request",
            Some(Box::new(Link("Connection refused", None))),
        ));
        assert_eq!(
            candidate_failure_label(&err),
            "connection error: error sending request: Connection refused"
        );
        let status = http_status(503);
        assert_eq!(candidate_failure_label(&status), status.to_string());
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
            exhausted_transient_label(&PromptError::MaxTurns {
                max_turns: 3,
                chat_history: Vec::new(),
                prompt: Message::user("hi"),
            })
            .is_none()
        );
    }

    /// The words of the report a user reads for a model call that showed them
    /// nothing, pinned because they are the user's whole account of it.
    /// Neither half has to have an answer: a missing finish reason costs one
    /// clause, and a missing ceiling says so in place of a number.
    #[test]
    fn the_textless_line_names_the_finish_reason_and_the_ceiling() {
        assert_eq!(
            textless_line(Some("length"), Some(2048)),
            "[outrig] the model produced no text and no tool call this turn \
             (provider finish reason: \"length\"); the request carried max-tokens = 2048."
        );
        assert_eq!(
            textless_line(Some("stop"), None),
            "[outrig] the model produced no text and no tool call this turn \
             (provider finish reason: \"stop\"); the request carried no max-tokens, so the \
             provider's own default applied."
        );
        assert_eq!(
            textless_line(None, Some(512)),
            "[outrig] the model produced no text and no tool call this turn; \
             the request carried max-tokens = 512."
        );
        assert_eq!(
            textless_line(None, None),
            "[outrig] the model produced no text and no tool call this turn; the request \
             carried no max-tokens, so the provider's own default applied."
        );
    }

    /// Only non-blank text or a tool call is something shown. Reasoning is not,
    /// and neither is the empty text part rig turns an empty provider turn into.
    #[test]
    fn only_non_blank_text_or_a_tool_call_is_something_shown() {
        let shown = |parts: Vec<AssistantContent>| showed_something(&parts);

        assert!(shown(vec![AssistantContent::text("done")]));
        assert!(shown(vec![AssistantContent::tool_call(
            "call-1",
            ToolName::new("shell__exec").expect("a name"),
            serde_json::json!({}),
        )]));
        assert!(shown(vec![
            AssistantContent::reasoning("weighing it"),
            AssistantContent::text("done"),
        ]));
        assert!(!shown(vec![AssistantContent::text("")]));
        assert!(!shown(vec![AssistantContent::text(" \n")]));
        assert!(!shown(vec![AssistantContent::reasoning("weighing it")]));
    }
}
