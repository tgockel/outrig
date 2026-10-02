//! Moving to the next candidate when one fails mid-round.
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
//! Failover inherits that unchanged. A round is a sequence of `completion()`
//! calls with Python run between them, so moving candidates *inside* one call
//! re-runs nothing, and moving them *around* the round would re-run code that
//! already ran in the interpreter.
//!
//! Four things follow from sitting there, and each is a design constraint
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
//! * **Each candidate has its own window.** The round's hook assembles a call's
//!   view for the head of the list. A move to another candidate assembles it
//!   again, from the store, against that candidate's [`Budget`] -- a smaller
//!   model is a smaller window, and what fit the head may not fit it.
//! * **Each candidate reads what the others wrote.** One model's reply joins
//!   the conversation every model is sent, so each candidate's view is
//!   assembled for its provider's protocol as well as its window: reasoning
//!   Anthropic cannot take back is left out of a call to it ([`Wire`]).
//! * **A failure must not disappear.** Each candidate's reason for being
//!   abandoned is kept, and exhausting the chain reports all of them together
//!   rather than only the last.
//!
//! Copied from `outrig-cli`'s `llm/failover.rs`, which keeps its own. Besides
//! the view, what changed in coming across is how the chain's state gets out.
//! The CLI recovers it by prefix-matching the error string a chain's exhaustion
//! is rendered into. Here a move is a `model.failover` event as it happens, and
//! the candidate that answered travels as the completion's response
//! ([`Answered`]), which rig hands the round's hook. Nothing parses an error.
//!
//! [`RetryingHttpClient`]: super::retry::RetryingHttpClient
//! [`RetryingModel`]: super::retry::RetryingModel
//! [`ChainDeadline`]: super::retry::ChainDeadline
//! [`retry`]: super::retry
//! [`Wire`]: super::budget::Wire

use std::sync::Arc;

use rig::OneOrMany;
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, Message,
};
use rig::streaming::StreamingCompletionResponse;
use serde::{Deserialize, Serialize};

use super::budget::Budget;
use super::history::{History, TooLarge};
use super::retry::RetryPolicy;
use crate::events::{Event, Events};

/// One candidate of a chain, behind an object-safe interface.
///
/// `CompletionModel` cannot be a trait object, so this is the reduction of it a
/// chain actually needs: run one completion, say whether native structured
/// output composes with tools, and carry what must be rewritten per candidate.
///
/// `Response` is erased to `()`. That is sound because nothing here ever
/// *reads* `CompletionResponse::raw_response` above the candidate -- the
/// textless-reply report reads it inside [`RetryingModel`], below the erasure.
/// The round reads `choice`, `usage`, and `message_id`, all of which survive.
///
/// [`RetryingModel`]: super::retry::RetryingModel
pub(crate) trait Candidate: Send + Sync {
    /// The wire identifier this candidate sends, which is not the model name --
    /// `anthropic.claude-opus-5-v1:0` is not `opus-5-bedrock`.
    fn model_identifier(&self) -> &str;

    /// What a call to this candidate may carry, and the output-token ceiling it
    /// carries, computed at build time through the same precedence for every
    /// candidate.
    ///
    /// The ceiling is the load-bearing per-candidate rewrite. It is folded into
    /// the agent at build time and capped, for Anthropic, against what the
    /// identifier publishes -- so the value baked into the request is the
    /// head's. Without the rewrite the identifier would move on a failover and
    /// the ceiling would not.
    fn budget(&self) -> &Budget;

    /// The concrete `[models.<name>]` row, for the per-candidate report and the
    /// move announcement. Never the alias's name.
    fn model_name(&self) -> &str {
        &self.budget().model
    }

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
    model_identifier: String,
    budget: Budget,
}

impl<M> ModelCandidate<M> {
    pub(crate) fn new(model: M, model_identifier: impl Into<String>, budget: Budget) -> Self {
        Self {
            model,
            model_identifier: model_identifier.into(),
            budget,
        }
    }
}

impl<M> Candidate for ModelCandidate<M>
where
    M: CompletionModel + Send + Sync + 'static,
{
    fn model_identifier(&self) -> &str {
        &self.model_identifier
    }

    fn budget(&self) -> &Budget {
        &self.budget
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
            // The erasure. Everything the round reads survives; only the
            // provider's untouched raw body is dropped -- see the trait's doc.
            Ok(CompletionResponse {
                choice: response.choice,
                usage: response.usage,
                raw_response: (),
                message_id: response.message_id,
            })
        })
    }
}

/// How a chain rebuilds a call's conversation for the candidate it moves to.
///
/// [`History`] is the one there is. The trait is what lets the chain's own
/// tests stand in for the store.
pub(crate) trait View: Send + Sync {
    /// The messages a call to a candidate held to `budget` is sent, oldest
    /// first, ending with the call's prompt -- or why its prompt does not fit.
    fn view(&self, budget: &Budget) -> Result<Vec<Message>, TooLarge>;
}

/// Assembled as the round's hook assembles the head's view, and recorded the
/// same way: a `model.call` manifest naming the candidate's budget.
impl View for History {
    fn view(&self, budget: &Budget) -> Result<Vec<Message>, TooLarge> {
        self.assemble(budget).map(|(sent, _)| sent)
    }
}

/// What a chain's completion carries in place of the provider's raw response:
/// the candidate that answered.
///
/// The round's hook reads it as rig hands the response over, which is how each
/// call is attributed to the model that answered it rather than to the head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Answered {
    /// The candidate's `[models.<name>]` row.
    pub(crate) model: String,
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
/// candidate. A `401` is not retryable -- but in a chain it is a reason to try
/// the next candidate, whose key may be fine. So is a call whose latest turn
/// does not fit the next candidate's window: that candidate is passed over for
/// the call, with the reason. What must not happen is a failure disappearing,
/// so every reason is kept and reported together on exhaustion.
///
/// Every agent's model is one of these. A chain of one returns its candidate's
/// error as it stands, and arms a deadline whose bound reproduces what the
/// per-attempt budget already gave.
///
/// `Clone` because rig's `CompletionModel` requires it and `Agent` clones the
/// model per request. Every field shares rather than rebuilds: the candidates
/// behind an `Arc`, since a boxed trait object cannot be cloned, and the
/// policy's own clone deliberately shares the chain deadline.
#[derive(Clone)]
pub(crate) struct FailoverModel {
    candidates: Arc<Vec<Box<dyn Candidate>>>,
    policy: RetryPolicy,
    /// ANDed across the chain, computed once at build time -- see
    /// [`composes_native_output_with_tools`].
    ///
    /// [`composes_native_output_with_tools`]: Self::composes_native_output_with_tools
    composes: bool,
    /// Where each move is recorded.
    events: Events,
    /// Where a moved call's view comes from.
    view: Arc<dyn View>,
}

impl FailoverModel {
    /// Build a chain over `candidates`, in preference order, recording each
    /// move in `events` and taking a moved call's view from `view`.
    ///
    /// `policy` is the one whose [`chain_deadline`] every candidate below
    /// already holds a handle to; arming it here is what bounds them all.
    ///
    /// [`chain_deadline`]: super::retry::RetryPolicy::chain_deadline
    pub(crate) fn new(
        candidates: Vec<Box<dyn Candidate>>,
        policy: RetryPolicy,
        events: Events,
        view: Arc<dyn View>,
    ) -> Self {
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
            events,
            view,
        }
    }

    /// What a call to each candidate may carry, in the chain's order: the
    /// head's first.
    pub(crate) fn budgets(&self) -> impl Iterator<Item = &Budget> {
        self.candidates.iter().map(|candidate| candidate.budget())
    }

    /// Say that `from` was abandoned for `error` and the call moves to `to`.
    ///
    /// Announced once per move, beside the retry lines. An alias already widens
    /// what a config typo can silently do; a chain that moves mid-round means
    /// one round can be half one model's work, which is the stronger version of
    /// the same hazard and wants the same mitigation.
    fn announce(&self, from: &str, to: &str, error: &CompletionError) {
        let error = error.to_string();
        tracing::warn!("model {from} failed ({error}); trying {to}");
        self.events.emit(Event::ModelFailover {
            from,
            to,
            error: &error,
        });
    }
}

/// The failure a chain returns when every candidate has been abandoned.
///
/// Rendered as one line per candidate, mirroring the shape resolution uses for
/// an alias with no selectable candidate: three candidates failing for three
/// different reasons is exactly the case a single-line error wastes an
/// afternoon on. It says whether any reason could clear without a change to the
/// config -- one vendor rate-limiting while another's key is revoked is still
/// worth waiting out, because the rate limit is the one that can lift.
fn exhausted(abandoned: Vec<Abandoned>) -> CompletionError {
    let recoverable = abandoned
        .iter()
        .any(|a| super::retry::is_recoverable(&a.error));
    let rows: Vec<_> = abandoned
        .iter()
        .map(|a| (a.model_name.as_str(), a.error.to_string()))
        .collect();
    let tried = super::resolve::render_candidate_reasons(&rows);
    let failed = if recoverable {
        "every model candidate failed"
    } else {
        "every model candidate failed terminally"
    };
    CompletionError::ProviderError(format!("{failed}; tried:\n{tried}"))
}

/// `history`, with the messages after the system prompt replaced by `view`.
///
/// rig moves an agent's preamble into the request's history as a leading
/// system message (rig-core 0.40, `CompletionRequestBuilder::build`), so a view
/// assembled from the store goes after it rather than in place of it. A view
/// with nothing in it -- reachable only outside a round, where nothing calls
/// the model -- leaves the history as it was.
fn after_system(history: &OneOrMany<Message>, view: Vec<Message>) -> OneOrMany<Message> {
    let system = history
        .iter()
        .take_while(|message| matches!(message, Message::System { .. }))
        .cloned();
    OneOrMany::many(system.chain(view)).unwrap_or_else(|_| history.clone())
}

impl CompletionModel for FailoverModel {
    type Response = Answered;
    type StreamingResponse = ();

    /// Erased, and for a stronger reason than `RetryingModel`'s.
    ///
    /// `RetryingModel::make` delegates to `M::make`: nothing here constructs a
    /// model through rig's client path, but it has a real `Client` to delegate
    /// with. A chain has none -- its candidates are heterogeneous, so there is
    /// no one client type that could build them -- so there is nothing to
    /// delegate to and `make` cannot be honored at all.
    type Client = ();

    /// Unreachable: every chain is built in `build_agent`, from candidates the
    /// provider clients already made. rig's trait requires the method, and a
    /// chain has no client type to build from -- see [`Self::Client`] -- so
    /// this panics rather than inventing an empty chain that would fail on its
    /// first call with a much worse message.
    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        unreachable!("a failover chain is built by build_agent, never through rig's client path")
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        // Armed per call, not per round or per session: a round is a sequence
        // of these with Python run between them, and each one gets a whole
        // budget to find a working candidate.
        self.policy.chain_deadline.arm(self.policy.budget);

        let mut abandoned: Vec<Abandoned> = Vec::new();
        // Kept while a later candidate may still need it, and moved into the
        // last one's attempt: a request holds the whole view, so a chain of
        // one never copies it.
        let mut request = Some(request);
        let last = self.candidates.len() - 1;
        // Every call starts at the head. The list is a preference order, and
        // sticking to candidate two for the rest of a session because of one
        // rate-limit window would silently downgrade the user's choice. The
        // cost is one attempt against a still-dead endpoint per call, which the
        // short pre-first-byte connect bound makes cheap.
        for (index, candidate) in self.candidates.iter().enumerate() {
            let mut attempt = if index == last {
                request.take()
            } else {
                request.clone()
            }
            .expect("taken only for the last candidate");
            // The head is sent the view the round assembled for it. Any other
            // candidate is sent one assembled for its own window, which may
            // leave out more -- or cannot hold the call's latest turn at all.
            let outcome = if index == 0 {
                Ok(())
            } else {
                self.view.view(candidate.budget()).map(|view| {
                    attempt.chat_history = after_system(&attempt.chat_history, view);
                })
            };
            // Both rewrites are per candidate. `max_tokens` is the load-bearing
            // one: the agent carries none of its own, so without this no
            // ceiling would be sent. `model` arrives `None` on the agent path
            // and each candidate's rig model already carries its own
            // identifier, so setting it is belt-and-braces against a future
            // rig that populates the field -- correct, and free.
            attempt.model = Some(candidate.model_identifier().to_string());
            attempt.max_tokens = candidate.budget().max_tokens.map(u64::from);

            let error = match outcome {
                // Not made: a call that cannot fit is terminal for this
                // candidate, which is what a `RequestError` is to
                // `is_recoverable`.
                Err(too_large) => CompletionError::RequestError(Box::new(too_large)),
                Ok(()) => match candidate.completion(attempt).await {
                    Ok(response) => {
                        return Ok(CompletionResponse {
                            choice: response.choice,
                            usage: response.usage,
                            raw_response: Answered {
                                model: candidate.model_name().to_string(),
                            },
                            message_id: response.message_id,
                        });
                    }
                    Err(error) => error,
                },
            };
            if let Some(next) = self.candidates.get(index + 1) {
                self.announce(candidate.model_name(), next.model_name(), &error);
            }
            abandoned.push(Abandoned {
                model_name: candidate.model_name().to_string(),
                error,
            });
        }

        // A chain of one returns its candidate's error exactly as it stands,
        // so the round reports what that one endpoint said. Only a real chain
        // gets the aggregate.
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
        // and buys nothing here: the agent loop never streams, so this exists
        // only because the trait requires it.
        Err(CompletionError::ProviderError(
            "streaming is not supported through a model chain".to_string(),
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rig::OneOrMany;
    use rig::completion::{AssistantContent, Usage};

    use super::*;

    /// What a call to a test's candidate may carry: a window of no consequence,
    /// and `max_tokens`.
    fn budget(model: &str, max_tokens: Option<u32>) -> Budget {
        Budget {
            max_tokens,
            ..Budget::with_room(model, 100_000)
        }
    }

    /// A candidate that answers however the test says, and counts its calls.
    struct Scripted {
        budget: Budget,
        /// One outcome per call; the last repeats.
        script: Vec<Result<&'static str, CompletionError>>,
        calls: Arc<AtomicUsize>,
        /// Every request this candidate was actually sent.
        ///
        /// Behind an `Arc` for the same reason `calls` is: a test clones the
        /// handle before the fixture is boxed into the chain, which is what
        /// lets it inspect a candidate it no longer owns.
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
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
                budget: budget(name, None),
                script,
                calls: Arc::new(AtomicUsize::new(0)),
                seen: Arc::default(),
                composes: true,
            }
        }

        fn with_max_tokens(mut self, max_tokens: u32) -> Self {
            self.budget = budget(&self.budget.model, Some(max_tokens));
            self
        }

        fn composing(mut self, composes: bool) -> Self {
            self.composes = composes;
            self
        }
    }

    impl Candidate for Scripted {
        fn model_identifier(&self) -> &str {
            &self.budget.model
        }

        fn budget(&self) -> &Budget {
            &self.budget
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
            self.seen.lock().expect("seen").push(request);
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

    /// A request as rig builds one for an agent with a preamble: the system
    /// prompt first, then the conversation.
    fn request() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: None,
            chat_history: OneOrMany::many([Message::system("preamble"), "hello".into()])
                .expect("two messages"),
            documents: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
        }
    }

    /// A store stand-in: the view for a budget names its model, and the
    /// budgets it was asked for are kept. `too_large_for` names a candidate
    /// whose window the call's latest turn does not fit.
    #[derive(Default)]
    struct Views {
        asked: Mutex<Vec<String>>,
        too_large_for: Option<&'static str>,
    }

    impl View for Views {
        fn view(&self, budget: &Budget) -> Result<Vec<Message>, TooLarge> {
            self.asked.lock().expect("asked").push(budget.model.clone());
            if self.too_large_for == Some(budget.model.as_str()) {
                return Err(TooLarge {
                    turn: Some(3),
                    round: 1,
                    estimate: 9_000,
                    budget: budget.clone(),
                });
            }
            Ok(vec![Message::user(format!(
                "the view for {}",
                budget.model
            ))])
        }
    }

    fn chain(candidates: Vec<Box<dyn Candidate>>) -> FailoverModel {
        chain_over(candidates, Arc::new(Views::default()), Events::off())
    }

    fn chain_over(
        candidates: Vec<Box<dyn Candidate>>,
        view: Arc<dyn View>,
        events: Events,
    ) -> FailoverModel {
        FailoverModel::new(candidates, RetryPolicy::default(), events, view)
    }

    /// The text of the `ProviderError` a chain's exhaustion is.
    fn exhaustion(err: CompletionError) -> String {
        match err {
            CompletionError::ProviderError(text) => text,
            other => panic!("not a chain's exhaustion: {other:?}"),
        }
    }

    /// The headline case: candidate one is down past its own retries, and the
    /// call completes on candidate two rather than ending the round -- which
    /// the response says.
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
        assert_eq!(
            reply.raw_response,
            Answered {
                model: "anthropic".into()
            },
            "the answer is attributed to the candidate that gave it"
        );
    }

    /// A `401` is terminal for one vendor and says nothing about the next, so it
    /// moves rather than ending the round.
    #[tokio::test]
    async fn a_terminal_failure_still_moves() {
        let reply = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(401))),
            Box::new(Scripted::ok("anthropic")),
        ])
        .completion(request())
        .await;

        assert!(
            reply.is_ok(),
            "a 401 on candidate one must not end the call"
        );
    }

    /// ...but a `401` everywhere still fails, and says no reason could clear.
    #[tokio::test]
    async fn a_terminal_failure_on_every_candidate_fails() {
        let err = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(401))),
            Box::new(Scripted::failing("anthropic", http_status(401))),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let text = exhaustion(err);
        assert!(
            text.starts_with("every model candidate failed terminally; tried:\n"),
            "a 401 everywhere is not an outage to wait out: {text}"
        );
        assert!(
            text.contains("bedrock") && text.contains("anthropic"),
            "{text}"
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

        let text = exhaustion(err);
        assert!(
            text.starts_with("every model candidate failed; tried:\n"),
            "one transient candidate is enough to make waiting worth it: {text}",
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

        let label = exhaustion(err);
        assert!(
            label.contains("bedrock") && label.contains("503"),
            "{label}"
        );
        assert!(
            label.contains("anthropic") && label.contains("empty body"),
            "{label}"
        );
    }

    /// A chain of one hands back its candidate's error *as it stands*, so the
    /// round reports what that one endpoint said.
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
    }

    /// The load-bearing per-candidate rewrite: after a move, candidate two's own
    /// ceiling reaches the wire, not candidate one's.
    #[tokio::test]
    async fn a_move_rewrites_max_tokens_for_the_new_candidate() {
        let second = Scripted::ok("anthropic").with_max_tokens(8_192);
        // Cloned out before the fixture is boxed, so the chain can own the
        // candidate while the test still sees what it was asked for.
        let seen = Arc::clone(&second.seen);

        chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(503)).with_max_tokens(32_768)),
            Box::new(second),
        ])
        .completion(request())
        .await
        .expect("the second candidate answers");

        let asked: Vec<_> = seen
            .lock()
            .expect("seen")
            .iter()
            .map(|request| request.max_tokens)
            .collect();
        assert_eq!(
            asked,
            vec![Some(8_192)],
            "the ceiling must travel with the identifier"
        );
    }

    /// The head is sent the view the round assembled for it. A move assembles
    /// the call's view again for the next candidate's window, after the system
    /// prompt rig put first.
    #[tokio::test]
    async fn a_move_sends_the_view_assembled_for_the_new_candidate() {
        let first = Scripted::failing("bedrock", http_status(503));
        let second = Scripted::ok("anthropic");
        let (first_seen, second_seen) = (Arc::clone(&first.seen), Arc::clone(&second.seen));
        let views = Arc::new(Views::default());

        chain_over(
            vec![Box::new(first), Box::new(second)],
            Arc::clone(&views) as Arc<dyn View>,
            Events::off(),
        )
        .completion(request())
        .await
        .expect("the second candidate answers");

        let history = |seen: &Mutex<Vec<CompletionRequest>>| -> Vec<Message> {
            seen.lock().expect("seen")[0]
                .chat_history
                .iter()
                .cloned()
                .collect()
        };
        assert_eq!(
            history(&first_seen),
            request().chat_history.into_iter().collect::<Vec<_>>()
        );
        assert_eq!(
            history(&second_seen),
            [
                Message::system("preamble"),
                Message::user("the view for anthropic")
            ]
        );
        assert_eq!(*views.asked.lock().expect("asked"), ["anthropic"]);
    }

    /// A candidate whose window the call's latest turn does not fit is passed
    /// over, not called, and its reason is reported with the rest.
    #[tokio::test]
    async fn a_candidate_too_small_for_the_call_is_passed_over() {
        let small = Scripted::ok("small");
        let small_calls = Arc::clone(&small.calls);
        let views = Arc::new(Views {
            too_large_for: Some("small"),
            ..Views::default()
        });

        let reply = chain_over(
            vec![
                Box::new(Scripted::failing("head", http_status(503))),
                Box::new(small),
                Box::new(Scripted::ok("large")),
            ],
            views,
            Events::off(),
        )
        .completion(request())
        .await
        .expect("the third candidate answers");

        assert_eq!(small_calls.load(Ordering::SeqCst), 0, "it was not called");
        assert_eq!(reply.raw_response.model, "large");

        let err = chain_over(
            vec![
                Box::new(Scripted::failing("head", http_status(503))),
                Box::new(Scripted::ok("small")),
            ],
            Arc::new(Views {
                too_large_for: Some("small"),
                ..Views::default()
            }),
            Events::off(),
        )
        .completion(request())
        .await
        .expect_err("no candidate could answer");
        let text = exhaustion(err);
        assert!(
            text.contains("small") && text.contains("turn 3 of round 1"),
            "{text}"
        );
    }

    /// Each move is recorded as it happens, with the reason -- not only
    /// announced -- and the last candidate's failure, which has nowhere to
    /// move to, is not a move.
    #[tokio::test]
    async fn each_move_is_recorded() {
        let dir = tempfile::tempdir().expect("a log dir");
        let events = crate::events::opened(dir.path()).await;
        chain_over(
            vec![
                Box::new(Scripted::failing("bedrock", http_status(401))),
                Box::new(Scripted::failing("anthropic", http_status(503))),
            ],
            Arc::new(Views::default()),
            events.clone(),
        )
        .completion(request())
        .await
        .expect_err("every candidate failed");
        events.close().await.expect("the log is finished");

        let records = crate::events::recorded(dir.path());
        let moves = crate::events::of_kind(&records, "model.failover");
        assert_eq!(moves.len(), 1, "{moves:#?}");
        assert_eq!(moves[0]["from"], "bedrock");
        assert_eq!(moves[0]["to"], "anthropic");
        assert!(
            moves[0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("401")),
            "{}",
            moves[0]
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

        let moved = model.completion(request()).await.expect("moves to second");
        let head = model.completion(request()).await.expect("head recovered");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the second call must try candidate one again"
        );
        assert_eq!(
            (moved.raw_response.model, head.raw_response.model),
            ("anthropic".to_string(), "bedrock".to_string())
        );
    }

    /// Streaming reaches no candidate at all: the agent loop never streams, and
    /// a chain refuses rather than fanning a stream out.
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
