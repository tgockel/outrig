//! Moving to the next candidate when one fails mid-turn.
//!
//! [`RetryingHttpClient`] and [`RetryingModel`] make one endpoint survive a bad
//! minute. Neither can help when the endpoint itself is the problem -- a rate
//! limit that outlasts the budget, an outage, a key that was revoked -- even
//! though `alias = ["opus-5-bedrock", "opus-5-anthropic"]` names two more
//! endpoints serving the same weights. Selecting between those happens once, at
//! resolve time, and is deliberately blind to whether an endpoint is *up*:
//! building a remote client does no network I/O. This module is what makes the
//! ordering matter at runtime.
//!
//! [`FailoverModel`] is the third sibling to the two retry layers, at the same
//! place and for the same reason. [`retry`]'s module doc gives the argument:
//!
//! > rig runs tools *between* `completion()` calls, never inside one, so
//! > replaying either a single request or a single model call re-executes no
//! > container tool call. Retrying the whole `agent.prompt(...)` would -- a turn
//! > that fails on a *later* model call has already run the tool calls from the
//! > earlier ones -- which is why neither layer is up there.
//!
//! Failover inherits that unchanged. A turn is a sequence of `completion()`
//! calls with container tool calls between them, so moving candidates *inside*
//! one call re-runs nothing, and moving them *around* the turn would re-run
//! side effects that already happened in a container. The obvious placement --
//! run the turn against candidate one, re-run it against candidate two -- is
//! the wrong one, and `tools_run_before_a_move_are_not_re_executed` pins it.
//!
//! Three things follow from sitting there, and each is a design constraint
//! rather than a detail:
//!
//! * **The candidates are heterogeneous.** An alias may span an OpenAI row and
//!   an Anthropic one, whose rig models are different concrete types with
//!   different associated types. `CompletionModel` is not object-safe -- a
//!   `Clone` supertrait, associated types, `impl Future` returns, a generic
//!   `make` -- so [`Candidate`] is the small object-safe trait that stands in
//!   front of them, and `FailoverModel` implements `CompletionModel` in terms
//!   of a `Vec` of those.
//! * **The budget has to be shared.** Three candidates each spending the full
//!   `retry-budget-secs` against a total outage is a worst case worse than no
//!   failover at all, so the chain arms one [`ChainDeadline`] per call and
//!   every candidate's retry loop below it observes the same bound.
//! * **A failure must not disappear.** Each candidate's reason for being
//!   abandoned is kept, and exhausting the chain reports all of them together
//!   rather than only the last.
//!
//! [`RetryingHttpClient`]: super::retry::RetryingHttpClient
//! [`RetryingModel`]: super::retry::RetryingModel
//! [`ChainDeadline`]: super::retry::ChainDeadline
//! [`retry`]: super::retry

use std::sync::Arc;

use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, PromptError,
};
use rig::streaming::StreamingCompletionResponse;

use super::retry::RetryPolicy;

/// One candidate of a chain, behind an object-safe interface.
///
/// `CompletionModel` cannot be a trait object, so this is the reduction of it a
/// chain actually needs: run one completion, say whether native structured
/// output composes with tools, and carry the two things that must be rewritten
/// per candidate.
///
/// `Response` is erased to `()`. That is sound because nothing in outrig ever
/// *reads* `CompletionResponse::raw_response` -- a property of outrig rather
/// than of rig, which is why it is stated here rather than assumed: the only
/// production construction is `llm/mistralrs.rs`'s, and it is never consumed,
/// while the two remote arms' `Response` types are rig's own and outrig never
/// touches them. The agent loop reads `choice`, `usage`, and `message_id`, all
/// of which survive.
pub(crate) trait Candidate: Send + Sync {
    /// The concrete `[models.<name>]` row, for the per-candidate report and the
    /// move announcement. Never the alias's name.
    fn model_name(&self) -> &str;

    /// The wire identifier this candidate sends, which is not the model name --
    /// `anthropic.claude-opus-5-v1:0` is not `opus-5-bedrock`.
    fn model_identifier(&self) -> &str;

    /// This candidate's own output-token ceiling, computed at build time
    /// through the same precedence the single-candidate path uses.
    ///
    /// The load-bearing per-candidate rewrite. The ceiling is folded into the
    /// agent at resolve time and capped, for Anthropic, against what the
    /// identifier publishes -- so the value baked into the request is candidate
    /// one's. Without this the identifier would move on a failover and the
    /// ceiling would not.
    fn max_tokens(&self) -> Option<u64>;

    /// Whether this candidate's native structured output composes with tools.
    fn composes_native_output_with_tools(&self) -> bool;

    /// Run one completion against this candidate.
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CompletionResponse<()>, CompletionError>> + Send + '_>,
    >;
}

/// A [`Candidate`] over any concrete rig [`CompletionModel`].
///
/// One generic adapter rather than an arm per provider: everything that differs
/// between an OpenAI candidate and an Anthropic one is already a value by the
/// time it gets here.
pub(crate) struct ModelCandidate<M> {
    model: M,
    model_name: String,
    model_identifier: String,
    max_tokens: Option<u64>,
}

impl<M> ModelCandidate<M> {
    pub(crate) fn new(
        model: M,
        model_name: impl Into<String>,
        model_identifier: impl Into<String>,
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            model_identifier: model_identifier.into(),
            max_tokens: max_tokens.map(u64::from),
        }
    }
}

impl<M> Candidate for ModelCandidate<M>
where
    M: CompletionModel + Send + Sync + 'static,
{
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn model_identifier(&self) -> &str {
        &self.model_identifier
    }

    fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    fn composes_native_output_with_tools(&self) -> bool {
        self.model.composes_native_output_with_tools()
    }

    fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CompletionResponse<()>, CompletionError>> + Send + '_>,
    > {
        Box::pin(async move {
            let response = self.model.completion(request).await?;
            // The erasure. Everything outrig reads survives; only the provider's
            // untouched raw body is dropped -- see the trait's doc comment.
            Ok(CompletionResponse {
                choice: response.choice,
                usage: response.usage,
                raw_response: (),
                message_id: response.message_id,
            })
        })
    }
}

/// Why one candidate was abandoned, kept so exhaustion can report every reason
/// rather than only the last one's.
struct Abandoned {
    model_name: String,
    error: CompletionError,
}

/// A [`CompletionModel`] over an ordered set of provider-equivalent candidates,
/// moving to the next when the current one's own retry stack has given up.
///
/// The move rule is **any `Err`**, including ones that are terminal for that
/// candidate. A `401` is not retryable and must keep ending the process -- but
/// in a chain it is a reason to try the next candidate, whose key may be fine.
/// What must not happen is a failure disappearing, so every reason is kept and
/// reported together on exhaustion.
///
/// A chain of one is not a special case in the code, and is byte-for-byte the
/// old behavior at runtime: one candidate, its error returned as it stands, and
/// a deadline whose bound reproduces what the per-attempt budget already gave.
///
/// `Clone` because rig's `CompletionModel` requires it and `Agent` clones the
/// model per request. Every field shares rather than rebuilds: the candidates
/// behind an `Arc` so cloning cannot re-pay a mistralrs weight load, and the
/// policy's own clone deliberately shares the chain deadline.
#[derive(Clone)]
pub struct FailoverModel {
    candidates: Arc<Vec<Box<dyn Candidate>>>,
    policy: RetryPolicy,
    /// ANDed across the chain, computed once at build time -- see
    /// [`composes_native_output_with_tools`].
    ///
    /// [`composes_native_output_with_tools`]: Self::composes_native_output_with_tools
    composes: bool,
}

impl FailoverModel {
    /// Build a chain over `candidates`, in preference order.
    ///
    /// `policy` is the one whose [`chain_deadline`] every candidate below
    /// already holds a handle to; arming it here is what bounds them all.
    ///
    /// [`chain_deadline`]: super::retry::RetryPolicy::chain_deadline
    pub(crate) fn new(candidates: Vec<Box<dyn Candidate>>, policy: RetryPolicy) -> Self {
        // ANDed rather than delegated to candidate one. rig resolves the output
        // mode *before* the call, so a move mid-call cannot re-resolve it, and
        // a chain that answered `true` on candidate one's behalf would promise
        // something candidate two may not honor. The conservative reduction is
        // the only answer a chain can give -- and a homogeneous chain (the
        // overwhelmingly common case: the same weights at three vendors) pays
        // nothing for it.
        let composes = candidates
            .iter()
            .all(|candidate| candidate.composes_native_output_with_tools());
        Self {
            candidates: Arc::new(candidates),
            policy,
            composes,
        }
    }
}

/// Marks a [`CompletionError::ProviderError`] as a whole chain's exhaustion
/// rather than one provider's own complaint.
///
/// A prefix, because `CompletionError` is rig's enum and has no variant for
/// this. Named once and shared by the constructor and the predicate, so the two
/// cannot drift into disagreeing about what a chain failure looks like.
const EXHAUSTED_PREFIX: &str = "every model candidate failed; tried:";

/// The same, when *no* candidate failed for a reason a resend could fix.
///
/// A separate marker rather than a flag inside the message, so the two are
/// distinguished by the same mechanism that distinguishes a chain failure from
/// a provider's own complaint, and a reader of the stderr text sees the
/// difference too.
const EXHAUSTED_TERMINAL_PREFIX: &str = "every model candidate failed terminally; tried:";

/// What [`chain_exhausted_label`] recovers from a chain's exhaustion.
pub struct ChainExhaustion<'a> {
    /// The rendered per-candidate report: one line each, name and reason.
    pub tried: &'a str,
    /// Whether *any* candidate failed for a reason a resend could fix.
    ///
    /// The chain asks of all its candidates the question a single-candidate
    /// turn asks of its one: if even one was a rate limit or an unusable body,
    /// waiting and resending is worth advising and the turn ends. If every one
    /// was terminal -- a revoked key on each vendor -- a resend is futile, and
    /// the failure has to end the process exactly as it does for a lone
    /// candidate.
    pub recoverable: bool,
}

/// The failure a chain returns when every candidate has been abandoned.
///
/// Rendered as one line per candidate, mirroring the shape the static half
/// already uses for an alias with no selectable candidate: three candidates
/// failing for three different reasons is exactly the case a single-line error
/// wastes an afternoon on.
fn exhausted(abandoned: Vec<Abandoned>) -> CompletionError {
    // Recoverable if *any* candidate was: one vendor rate-limiting while
    // another's key is revoked is still worth waiting out, because the rate
    // limit is the one that can lift. Only when every reason is terminal is a
    // resend futile.
    let recoverable = abandoned
        .iter()
        .any(|a| super::retry::is_recoverable(&a.error));
    let rows: Vec<_> = abandoned
        .iter()
        .map(|a| (a.model_name.as_str(), a.error.to_string()))
        .collect();
    let tried = super::render_candidate_reasons(&rows);
    let prefix = if recoverable {
        EXHAUSTED_PREFIX
    } else {
        EXHAUSTED_TERMINAL_PREFIX
    };
    CompletionError::ProviderError(format!("{prefix}\n{tried}"))
}

impl CompletionModel for FailoverModel {
    type Response = ();
    type StreamingResponse = ();

    /// Erased along with `Response`, and for a stronger reason than
    /// `RetryingModel`'s.
    ///
    /// `RetryingModel::make` delegates to `M::make`: outrig never constructs a
    /// model through rig's client path, but it has a real `Client` to delegate
    /// with. A chain has none -- its candidates are heterogeneous, so there is
    /// no one client type that could build them -- so there is nothing to
    /// delegate to and `make` cannot be honored at all.
    type Client = ();

    /// Unreachable: outrig builds every chain in `build_agent`, from candidates
    /// the provider clients already made. rig's trait requires the method, and
    /// a chain has no client type to build from -- see [`Self::Client`] -- so
    /// this panics rather than inventing an empty chain that would fail on its
    /// first turn with a much worse message.
    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unreachable!("a failover chain is built by build_agent, never through rig's client path")
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Armed per call, not per turn or per session: a turn is a sequence of
        // these with tool calls between them, and each one gets a whole budget
        // to find a working candidate.
        self.policy.chain_deadline.arm(self.policy.budget);

        let mut abandoned: Vec<Abandoned> = Vec::new();
        // Every call starts at the head. The list is a preference order, and
        // sticking to candidate two for the rest of a session because of one
        // rate-limit window would silently downgrade the user's choice. The
        // cost is one attempt against a still-dead endpoint per call, which the
        // short pre-first-byte connect bound makes cheap.
        for (index, candidate) in self.candidates.iter().enumerate() {
            let mut attempt = request.clone();
            // Both rewrites are per candidate. `max_tokens` is the load-bearing
            // one: it is folded in at build time, so without this the ceiling
            // would stay candidate one's while the identifier moved. `model`
            // arrives `None` on the agent path and each candidate's rig model
            // already carries its own identifier, so setting it is
            // belt-and-braces against a future rig that populates the field --
            // correct, and free.
            attempt.model = Some(candidate.model_identifier().to_string());
            attempt.max_tokens = candidate.max_tokens();

            match candidate.completion(attempt).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // Announced once per move, on stderr beside the retry
                    // lines. An alias already widens what a config typo can
                    // silently do; a chain that moves mid-turn means one reply
                    // can be half one model's work, which is the stronger
                    // version of the same hazard and wants the same mitigation.
                    if let Some(next) = self.candidates.get(index + 1) {
                        eprintln!(
                            "[outrig] model {} failed ({error}); trying {}",
                            candidate.model_name(),
                            next.model_name(),
                        );
                    }
                    abandoned.push(Abandoned {
                        model_name: candidate.model_name().to_string(),
                        error,
                    });
                }
            }
        }

        // A chain of one returns its candidate's error exactly as it stands, so
        // every existing classification -- `exhausted_transient_label`, a `401`
        // ending the process -- still sees the error it has always seen. Only a
        // real chain gets the aggregate.
        match <[Abandoned; 1]>::try_from(abandoned) {
            Ok([only]) => Err(only.error),
            Err(many) => Err(exhausted(many)),
        }
    }

    async fn stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        // No failover, and no delegation either. Unifying `StreamingResponse`
        // across heterogeneous candidates costs an erasure of the whole stream
        // and buys nothing here: outrig's remote turns are non-streaming, and
        // the streaming arm is mistralrs-only -- the one style with no endpoint
        // to fail over from, and which `build_agent` never wraps in a chain.
        // Matches `RetryingModel::stream`'s reasoning and the precedent in
        // `plan/next/streaming-path-has-no-http-retry.md`.
        Err(CompletionError::ProviderError(
            "streaming is not supported through a model alias chain".to_string(),
        ))
    }

    /// ANDed across every candidate, computed once in [`FailoverModel::new`].
    ///
    /// Not delegated to the current candidate, the way `RetryingModel`
    /// delegates to its inner model: rig resolves the output mode before the
    /// call, and a move mid-call cannot re-resolve it.
    fn composes_native_output_with_tools(&self) -> bool {
        self.composes
    }
}

/// Did every candidate of a chain fail? Returns the rendered per-candidate
/// report if so.
///
/// The chain is a third retry layer, and `exhausted_transient_label`'s
/// soundness claim is that there is exactly one. This is what keeps that claim
/// true rather than merely still-compiling: a multi-candidate exhaustion is
/// *this* class, not that one, so what reaches the transient predicate still
/// means "the one endpoint this turn had stayed broken for its whole budget".
pub fn chain_exhausted_label(err: &PromptError) -> Option<ChainExhaustion<'_>> {
    let PromptError::CompletionError(CompletionError::ProviderError(message)) = err else {
        return None;
    };
    let (tried, recoverable) = match message.strip_prefix(EXHAUSTED_PREFIX) {
        Some(tried) => (tried, true),
        None => (message.strip_prefix(EXHAUSTED_TERMINAL_PREFIX)?, false),
    };
    Some(ChainExhaustion {
        tried: tried.strip_prefix('\n').unwrap_or(tried),
        recoverable,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rig::OneOrMany;
    use rig::completion::{AssistantContent, Usage};

    use super::*;

    /// A candidate that answers however the test says, and counts its calls.
    struct Scripted {
        name: &'static str,
        max_tokens: Option<u64>,
        /// One outcome per call; the last repeats.
        script: Vec<Result<&'static str, CompletionError>>,
        calls: Arc<AtomicUsize>,
        /// Every `max_tokens` this candidate was actually asked for.
        ///
        /// Behind an `Arc` for the same reason `calls` is: a test clones the
        /// handle before the fixture is boxed into the chain, which is what
        /// lets it inspect a candidate it no longer owns.
        seen_max_tokens: Arc<std::sync::Mutex<Vec<Option<u64>>>>,
        composes: bool,
    }

    impl Scripted {
        fn ok(name: &'static str) -> Self {
            Self::new(name, vec![Ok("hi")])
        }

        fn failing(name: &'static str, error: CompletionError) -> Self {
            Self::new(name, vec![Err(error)])
        }

        fn new(name: &'static str, script: Vec<Result<&'static str, CompletionError>>) -> Self {
            Self {
                name,
                max_tokens: None,
                script,
                calls: Arc::new(AtomicUsize::new(0)),
                seen_max_tokens: Arc::new(std::sync::Mutex::new(Vec::new())),
                composes: true,
            }
        }

        fn with_max_tokens(mut self, max_tokens: u64) -> Self {
            self.max_tokens = Some(max_tokens);
            self
        }

        fn composing(mut self, composes: bool) -> Self {
            self.composes = composes;
            self
        }
    }

    impl Candidate for Scripted {
        fn model_name(&self) -> &str {
            self.name
        }

        fn model_identifier(&self) -> &str {
            self.name
        }

        fn max_tokens(&self) -> Option<u64> {
            self.max_tokens
        }

        fn composes_native_output_with_tools(&self) -> bool {
            self.composes
        }

        fn completion(
            &self,
            request: CompletionRequest,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<CompletionResponse<()>, CompletionError>> + Send + '_>,
        > {
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_max_tokens
                .lock()
                .expect("seen")
                .push(request.max_tokens);
            let outcome = match self.script.get(nth).or_else(|| self.script.last()) {
                Some(Ok(text)) => Ok(*text),
                Some(Err(e)) => Err(clone_error(e)),
                None => Err(CompletionError::ProviderError("script empty".into())),
            };
            Box::pin(async move {
                outcome.map(|text| CompletionResponse {
                    choice: OneOrMany::one(AssistantContent::text(text)),
                    usage: Usage::new(),
                    raw_response: (),
                    message_id: None,
                })
            })
        }
    }

    /// `CompletionError` is not `Clone`, and the *variant* matters here -- a
    /// chain of one has to hand its candidate's error back unchanged, which a
    /// fixture that flattened everything to `ProviderError` could not observe.
    fn clone_error(e: &CompletionError) -> CompletionError {
        match e {
            CompletionError::ResponseError(m) => CompletionError::ResponseError(m.clone()),
            CompletionError::HttpError(rig::http_client::Error::InvalidStatusCode(status)) => {
                CompletionError::HttpError(rig::http_client::Error::InvalidStatusCode(*status))
            }
            other => CompletionError::ProviderError(other.to_string()),
        }
    }

    fn http_status(code: u16) -> CompletionError {
        CompletionError::HttpError(rig::http_client::Error::InvalidStatusCode(
            reqwest::StatusCode::from_u16(code).expect("status"),
        ))
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::one("hello".into()),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    fn chain(candidates: Vec<Box<dyn Candidate>>) -> FailoverModel {
        FailoverModel::new(candidates, RetryPolicy::default())
    }

    /// The headline case: candidate one is down past its own retries, and the
    /// turn completes on candidate two rather than ending.
    #[tokio::test]
    async fn a_failed_candidate_moves_to_the_next() {
        let first = Box::new(Scripted::failing("bedrock", http_status(503)));
        let second = Scripted::ok("anthropic");
        let second_calls = Arc::clone(&second.calls);

        let reply = chain(vec![first, Box::new(second)])
            .completion(request())
            .await
            .expect("the second candidate answers");

        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
        assert!(format!("{:?}", reply.choice).contains("hi"));
    }

    /// A `401` is terminal for one vendor and says nothing about the next, so it
    /// moves rather than ending the session.
    #[tokio::test]
    async fn a_terminal_failure_still_moves() {
        let reply = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(401))),
            Box::new(Scripted::ok("anthropic")),
        ])
        .completion(request())
        .await;

        assert!(reply.is_ok(), "a 401 on candidate one must not end the turn");
    }

    /// ...but a `401` everywhere still fails, rather than inviting a resend of a
    /// prompt no key can satisfy.
    #[tokio::test]
    async fn a_terminal_failure_on_every_candidate_fails() {
        let err = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(401))),
            Box::new(Scripted::failing("anthropic", http_status(401))),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let err = PromptError::CompletionError(err);
        let chain = chain_exhausted_label(&err).expect("a chain failure is reported as one");
        assert!(chain.tried.contains("bedrock"), "{}", chain.tried);
        assert!(chain.tried.contains("anthropic"), "{}", chain.tried);
        assert!(
            !chain.recoverable,
            "a 401 everywhere is not an outage to wait out -- reporting it as \
             recoverable would invite a resend of a prompt no key can satisfy",
        );
    }

    /// One terminal candidate does not make the chain terminal. A revoked key
    /// on one vendor while another is rate-limited is still worth waiting out:
    /// the rate limit is the reason that can lift on its own.
    #[tokio::test]
    async fn a_mixed_exhaustion_stays_recoverable() {
        let err = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(401))),
            Box::new(Scripted::failing("anthropic", http_status(503))),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let err = PromptError::CompletionError(err);
        let chain = chain_exhausted_label(&err).expect("a chain failure is reported as one");
        assert!(
            chain.recoverable,
            "one transient candidate is enough to make the resend advice honest",
        );
    }

    /// Exhaustion names every candidate with its *own* reason, not just the last
    /// one's -- the case a single-line error wastes an afternoon on.
    #[tokio::test]
    async fn exhaustion_reports_every_candidate_and_its_own_reason() {
        let err = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(503))),
            Box::new(Scripted::failing(
                "anthropic",
                CompletionError::ResponseError("empty body".into()),
            )),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let label = chain_exhausted_label(&PromptError::CompletionError(err))
            .expect("reported as a chain failure")
            .tried
            .to_string();
        assert!(label.contains("bedrock") && label.contains("503"), "{label}");
        assert!(
            label.contains("anthropic") && label.contains("empty body"),
            "{label}"
        );
    }

    /// A chain of one hands back its candidate's error *as it stands*, so
    /// `exhausted_transient_label` and the `401`-ends-the-process path see
    /// exactly the error they have always seen.
    #[tokio::test]
    async fn a_single_candidate_chain_returns_its_error_unwrapped() {
        let err = chain(vec![Box::new(Scripted::failing("only", http_status(503)))])
            .completion(request())
            .await
            .expect_err("the one candidate failed");

        assert!(
            matches!(err, CompletionError::HttpError(_)),
            "a chain of one must not wrap its candidate's error: {err:?}"
        );
        let prompt_err = PromptError::CompletionError(err);
        assert!(chain_exhausted_label(&prompt_err).is_none());
        assert!(
            super::super::retry::exhausted_transient_label(&prompt_err).is_some(),
            "the single-candidate path keeps its transient classification"
        );
    }

    /// The load-bearing per-candidate rewrite: after a move, candidate two's own
    /// ceiling reaches the wire, not candidate one's.
    #[tokio::test]
    async fn a_move_rewrites_max_tokens_for_the_new_candidate() {
        let second = Scripted::ok("anthropic").with_max_tokens(8_192);
        // Cloned out before the fixture is boxed, so the chain can own the
        // candidate while the test still sees what it was asked for.
        let seen = Arc::clone(&second.seen_max_tokens);

        chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(503)).with_max_tokens(32_768)),
            Box::new(second),
        ])
        .completion(request())
        .await
        .expect("the second candidate answers");

        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Some(8_192)],
            "the ceiling must travel with the identifier"
        );
    }

    /// A chain cannot delegate this: rig resolves the output mode before the
    /// call, so a disagreement reduces to the safe answer.
    #[test]
    fn disagreeing_candidates_resolve_to_false() {
        let mixed = chain(vec![
            Box::new(Scripted::ok("a").composing(true)),
            Box::new(Scripted::ok("b").composing(false)),
        ]);
        assert!(!mixed.composes_native_output_with_tools());

        let homogeneous = chain(vec![
            Box::new(Scripted::ok("a").composing(true)),
            Box::new(Scripted::ok("b").composing(true)),
        ]);
        assert!(
            homogeneous.composes_native_output_with_tools(),
            "a homogeneous chain pays nothing for the reduction"
        );
    }

    /// Every call starts at the head, so one rate-limit window does not demote
    /// the preferred vendor for the rest of the session.
    #[tokio::test]
    async fn every_call_starts_at_the_head_of_the_list() {
        // Fails once, then recovers.
        let first = Scripted::new("bedrock", vec![Err(http_status(429)), Ok("recovered")]);
        let calls = Arc::clone(&first.calls);
        let model = chain(vec![Box::new(first), Box::new(Scripted::ok("anthropic"))]);

        model.completion(request()).await.expect("moves to second");
        model.completion(request()).await.expect("head recovered");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the second call must try candidate one again"
        );
    }

    /// Streaming reaches no candidate at all; the mistralrs arm, which is the
    /// only streaming one, is never wrapped in a chain.
    #[tokio::test]
    async fn streaming_is_refused_rather_than_fanned_out() {
        let candidate = Scripted::ok("only");
        let calls = Arc::clone(&candidate.calls);
        assert!(
            chain(vec![Box::new(candidate)])
                .stream(request())
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
