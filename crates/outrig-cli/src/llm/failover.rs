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
//! Four things follow from sitting there, and each is a design constraint
//! rather than a detail:
//!
//! * **The chain is the agent's model.** rig's runner calls its model through
//!   a handler it dispatches completions to, and lets the host register that
//!   handler ([`rig::agent::AgentBuilder::over_handlers`]). `FailoverModel` is
//!   that handler ([`Serve`]): it answers each completion the runner
//!   dispatches with the reply of whichever candidate answered, as that
//!   candidate's wire decoded it, so the reply keeps the provider and model it
//!   came from -- which is what lets a later call replay the model's thinking.
//!   Every agent's model is one, of one candidate when no alias names more.
//! * **The candidates are heterogeneous.** An alias may span an OpenAI row and
//!   an Anthropic one. rig erases a model's wire and transport behind a
//!   [`DynModel`](rig::DynModel), and [`Candidate`] is the small trait that
//!   stands in front of that with what a chain rewrites per candidate.
//! * **The budget has to be shared.** Three candidates each spending the full
//!   `retry-budget-secs` against a total outage is a worst case worse than no
//!   failover at all, so the chain arms one [`ChainDeadline`] per call and
//!   every candidate's retry loop below it observes the same bound.
//! * **A failure must not disappear.** Each candidate's reason for being
//!   abandoned is kept, and exhausting the chain reports all of them together
//!   rather than only the last. What leaves the chain is the report rig hands
//!   the turn ([`report`]), which names the failure's class by a code, so the
//!   REPL reads a class rather than parsing an error's text.
//! * **Neither is a refusal of `drop_block`.** rig asks Anthropic's API to
//!   drop thinking a change to the conversation made stale, for a model its
//!   catalog says binds its thinking to the conversation, under the beta that
//!   takes the request. An endpoint at a custom `base-url` that refuses the
//!   request for asking -- a gateway that does not know the beta -- is sent the
//!   call again through the same model built not to ask, and every later call
//!   to it goes without ([`Candidate::unbind`]).
//!
//! [`RetryingHttpClient`]: super::retry::RetryingHttpClient
//! [`RetryingModel`]: super::retry::RetryingModel
//! [`ChainDeadline`]: super::retry::ChainDeadline
//! [`retry`]: super::retry

// rig's `ProviderError` is what each candidate hands back by value, and what
// the chain carries from one to the next; boxing it in OutRig's own functions
// would only move the size.
#![allow(clippy::result_large_err)]

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rig::completion::{
    CompletionRequest, CompletionResponse, ModelRef, PromptError, ProviderCapabilities,
};
use rig::effect::{Answered, EffectKind, FamilyDescriptor, HandlerDescriptor, family, model_key};
use rig::error::{ErrorKind, ErrorReport, ProviderError};
use rig::serve::{Dispatch, Reply, Serve};

use super::retry::{self, RetryPolicy, RetryingModel, candidate_failure_label};

/// The label the chain is registered under in the agent's handler table, which
/// is what rig names the agent's model.
pub(crate) const CHAIN_LABEL: &str = "outrig";

/// The code a report carries when every candidate of a chain was abandoned,
/// and the report's `retryable` says whether any reason could clear.
pub(crate) const CODE_EXHAUSTED: &str = "outrig.exhausted";

/// One candidate of a chain, behind an object-safe interface: run one
/// completion, say what the provider composes, and carry what must be
/// rewritten per candidate.
pub(crate) trait Candidate: Send + Sync {
    /// The concrete `[models.<name>]` row, for the per-candidate report and the
    /// move announcement. Never the alias's name.
    fn model_name(&self) -> &str;

    /// The wire identifier this candidate sends, which is not the model name --
    /// `anthropic.claude-opus-5-v1:0` is not `opus-5-bedrock`.
    fn model_identifier(&self) -> &str;

    /// This candidate's own output-token ceiling, computed at build time
    /// through the same precedence for every candidate.
    ///
    /// The load-bearing per-candidate rewrite. The ceiling is capped, for
    /// Anthropic, against what the identifier publishes -- so without this the
    /// identifier would move on a failover and the ceiling would not.
    fn max_tokens(&self) -> Option<u64>;

    /// What this candidate's provider composes: whether native structured
    /// output composes with tools, and whether a forced tool choice is refused.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Stop asking the API to drop thinking a change to the conversation made
    /// stale (`drop_block`), because this candidate's endpoint refused a
    /// request that asked: whether it was asking.
    fn unbind(&self) -> bool {
        false
    }

    /// Run one completion against this candidate.
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + '_>,
    >;
}

/// A [`Candidate`] over a provider's model, its wire erased.
///
/// One adapter rather than an arm per provider: everything that differs
/// between an OpenAI candidate and an Anthropic one is already a value by the
/// time it gets here.
pub(crate) struct ModelCandidate {
    model: RetryingModel,
    model_name: String,
    model_identifier: String,
    max_tokens: Option<u64>,
    /// For an Anthropic candidate, the same model built not to ask for
    /// `drop_block`, which its calls go through once its endpoint has refused
    /// that.
    binding: Option<Binding>,
}

impl ModelCandidate {
    pub(crate) fn new(
        model: RetryingModel,
        model_name: impl Into<String>,
        model_identifier: impl Into<String>,
        max_tokens: Option<u32>,
        binding: Option<Binding>,
    ) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            model_identifier: model_identifier.into(),
            max_tokens: max_tokens.map(u64::from),
            binding,
        }
    }
}

/// A candidate's model built not to ask for `drop_block` -- on a dialect rig
/// does not bind thinking for, so neither the field nor its beta is sent --
/// and whether its endpoint has refused the asking, which is when calls go
/// through it: see [`Candidate::unbind`]. A request has no headers of its own,
/// so going without the beta is going through another model.
pub(crate) struct Binding {
    unbound: RetryingModel,
    refused: AtomicBool,
}

impl Binding {
    pub(crate) fn new(unbound: RetryingModel) -> Self {
        Self {
            unbound,
            refused: AtomicBool::new(false),
        }
    }
}

impl Candidate for ModelCandidate {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn model_identifier(&self) -> &str {
        &self.model_identifier
    }

    fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.model.capabilities()
    }

    fn unbind(&self) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|binding| !binding.refused.swap(true, Ordering::Relaxed))
    }

    fn completion(
        &self,
        request: CompletionRequest,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + '_>,
    > {
        let model = match &self.binding {
            Some(binding) if binding.refused.load(Ordering::Relaxed) => &binding.unbound,
            _ => &self.model,
        };
        Box::pin(model.completion(request))
    }
}

/// Why one candidate was abandoned, kept so exhaustion can report every reason
/// rather than only the last one's.
#[derive(Debug)]
struct Abandoned {
    model_name: String,
    error: ProviderError,
}

/// The agent's model: an ordered set of provider-equivalent candidates, moving
/// to the next when the current one's own retry stack has given up.
///
/// The move rule is **any `Err`**, including ones that are terminal for that
/// candidate. A `401` is not retryable and must keep ending the process -- but
/// in a chain it is a reason to try the next candidate, whose key may be fine.
/// What must not happen is a failure disappearing, so every reason is kept and
/// reported together on exhaustion.
///
/// A chain of one is not a special case in the code: one candidate, its error
/// returned as it stands, and a deadline whose bound reproduces what the
/// per-attempt budget already gave.
///
/// `Clone` so the agent and its builder can both hold it: the agent's handler
/// table owns one, and what the turn reads of the chain -- who answered -- is
/// read through another. Every field shares rather than rebuilds: the
/// candidates behind an `Arc`, since a boxed trait object cannot be cloned,
/// and the policy's own clone deliberately shares the chain deadline and the
/// record of who answered.
#[derive(Clone)]
pub struct FailoverModel {
    candidates: Arc<Vec<Box<dyn Candidate>>>,
    policy: RetryPolicy,
    /// Indices into `candidates` that returned `Ok` since the last
    /// [`take_answered`](Self::take_answered), for attribution after the fact
    /// (#265).
    ///
    /// A set, because every call starts again at the head: one round of several
    /// calls can be answered by the head, by a fallback, or by both, and the
    /// latest call's candidate says nothing of the others. Shared across the
    /// clones, as the policy's chain deadline is, so whoever holds the model
    /// sees what any clone recorded. Never held across an await.
    answered: Arc<Mutex<BTreeSet<usize>>>,
    /// Reduced across the chain, computed once at build time -- see
    /// [`FailoverModel::new`].
    capabilities: ProviderCapabilities,
}

impl FailoverModel {
    /// Build a chain over `candidates`, in preference order.
    ///
    /// `policy` is the one whose [`chain_deadline`] every candidate below
    /// already holds a handle to; arming it here is what bounds them all.
    ///
    /// [`chain_deadline`]: super::retry::RetryPolicy::chain_deadline
    pub(crate) fn new(candidates: Vec<Box<dyn Candidate>>, policy: RetryPolicy) -> Self {
        // Reduced rather than delegated to candidate one. rig resolves the
        // output mode *before* the call, so a move mid-call cannot re-resolve
        // it, and a chain that answered for candidate one would promise
        // something candidate two may not honor. The conservative reduction is
        // the only answer a chain can give -- composition only when every
        // candidate composes, a refusal when any refuses -- and a homogeneous
        // chain (the overwhelmingly common case: the same weights at three
        // vendors) pays nothing for it.
        let each: Vec<_> = candidates.iter().map(|c| c.capabilities()).collect();
        let capabilities = ProviderCapabilities::new()
            .with_native_output_tool_composition(
                each.iter().all(|c| c.composes_native_output_with_tools),
            )
            .with_forced_tool_choice_rejected(each.iter().any(|c| c.rejects_forced_tool_choice));
        Self {
            candidates: Arc::new(candidates),
            policy,
            answered: Arc::default(),
            capabilities,
        }
    }

    /// The candidates that returned `Ok` to a `completion()` call since this was
    /// last asked, as indices into the list the chain was built from, ascending.
    ///
    /// A call that failed on every candidate answered nothing and is not here.
    pub(crate) fn take_answered(&self) -> Vec<usize> {
        std::mem::take(&mut *self.answered.lock().expect("answered record"))
            .into_iter()
            .collect()
    }

    /// One completion: each candidate in turn, until one answers.
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
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
            let error = loop {
                let mut attempt = request.clone();
                // Both rewrites are per candidate. `max_tokens` is the
                // load-bearing one: the agent carries none of its own, so
                // without this no ceiling would be sent. `model` arrives `None`
                // on the agent path and each candidate's rig model already
                // carries its own identifier, so setting it is belt-and-braces
                // against a future rig that populates the field -- correct,
                // and free.
                attempt.model = Some(candidate.model_identifier().to_string());
                attempt.max_tokens = candidate.max_tokens();

                match candidate.completion(attempt).await {
                    Ok(response) => {
                        self.answered
                            .lock()
                            .expect("answered record")
                            .insert(index);
                        // As the candidate's wire decoded it, its provider and
                        // model kept.
                        return Ok(response);
                    }
                    Err(error) => {
                        // Refused for asking that stale thinking be dropped:
                        // sent again without asking, as every later call to it
                        // is.
                        if refuses_binding(&error) && candidate.unbind() {
                            eprintln!(
                                "[outrig] model {} refused the request that asked for \
                                 drop_block ({}); sending it and every later call without \
                                 asking",
                                candidate.model_name(),
                                candidate_failure_label(&error),
                            );
                            continue;
                        }
                        break error;
                    }
                }
            };
            // Announced once per move, on stderr beside the retry lines. An
            // alias already widens what a config typo can silently do; a chain
            // that moves mid-turn means one reply can be half one model's
            // work, which is the stronger version of the same hazard and wants
            // the same mitigation.
            if let Some(next) = self.candidates.get(index + 1) {
                eprintln!(
                    "[outrig] model {} failed ({}); trying {}",
                    candidate.model_name(),
                    candidate_failure_label(&error),
                    next.model_name(),
                );
            }
            abandoned.push(Abandoned {
                model_name: candidate.model_name().to_string(),
                error,
            });
        }

        // A chain of one returns its candidate's error exactly as it stands, so
        // its report names the class that one endpoint's failure was. Only a
        // real chain gets the aggregate.
        match <[Abandoned; 1]>::try_from(abandoned) {
            Ok([only]) => Err(only.error),
            Err(many) => Err(Exhausted::into_error(many)),
        }
    }
}

impl Serve for FailoverModel {
    type Family = family::Completion;

    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: model_key(CHAIN_LABEL),
            family: FamilyDescriptor::Completion {
                model: ModelRef::new(CHAIN_LABEL),
                capabilities: self.capabilities,
            },
            layers: Vec::new(),
        }
    }

    /// Answer the one completion rig's runner dispatches per model call with
    /// the chain's, as a report when it fails. The runner never streams an
    /// awaited prompt, and the chain streams nothing: unifying a stream across
    /// heterogeneous candidates would erase the whole stream and buy nothing.
    async fn serve(&self, kind: EffectKind, _dispatch: Dispatch) -> Reply {
        let outcome = match kind {
            EffectKind::Completion {
                request,
                stream: false,
            } => self
                .completion(Arc::unwrap_or_clone(request))
                .await
                .map(|response| Answered::Completion(Arc::new(response)))
                .map_err(report),
            EffectKind::Completion { stream: true, .. } => Err(ErrorReport::new(
                ErrorKind::Request,
                "streaming is not supported through a model alias chain",
            )),
            other => Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                format!("a model chain serves completions, not {}", other.name()),
            )),
        };
        Reply::Outcome(outcome)
    }
}

/// Opens the message of a chain's exhaustion report, when a resend could fix
/// at least one candidate's reason.
///
/// Named once and shared by the renderer and the reader, so the two cannot
/// drift into disagreeing about what a chain failure looks like.
const EXHAUSTED_PREFIX: &str = "every model candidate failed; tried:";

/// The same, when *no* candidate failed for a reason a resend could fix.
///
/// A separate wording rather than a flag inside the message, so a reader of
/// the stderr text sees the difference too.
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

/// The failure a chain returns when every candidate has been abandoned, carried
/// inside [`ProviderError::Request`] so the chain can [read it back](Self::of)
/// into the report it hands rig instead of parsing its text.
///
/// Rendered as one line per candidate, mirroring the shape the static half
/// already uses for an alias with no selectable candidate: three candidates
/// failing for three different reasons is exactly the case a single-line error
/// wastes an afternoon on.
#[derive(Debug)]
struct Exhausted {
    /// Every candidate, in the chain's order: the head's reason first.
    abandoned: Vec<Abandoned>,
}

impl Exhausted {
    fn into_error(abandoned: Vec<Abandoned>) -> ProviderError {
        // By value: a boxed reason would be boxed again, as a `Box<Self>`
        // rather than a `Self`, and `of` could not read it back.
        ProviderError::request(Self { abandoned })
    }

    /// The exhaustion `err` is, if it is one.
    fn of(err: &ProviderError) -> Option<&Self> {
        match err {
            ProviderError::Request(inner) => (**inner).downcast_ref(),
            _ => None,
        }
    }

    /// Whether at least one candidate failed for a reason a resend could fix:
    /// one vendor rate-limiting while another's key is revoked is still worth
    /// waiting out, because the rate limit is the one that can lift. Only when
    /// every reason is terminal is a resend futile.
    fn may_clear(&self) -> bool {
        self.abandoned.iter().any(|a| candidate_may_clear(&a.error))
    }
}

impl fmt::Display for Exhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: Vec<_> = self
            .abandoned
            .iter()
            .map(|a| (a.model_name.as_str(), candidate_failure_label(&a.error)))
            .collect();
        let prefix = if self.may_clear() {
            EXHAUSTED_PREFIX
        } else {
            EXHAUSTED_TERMINAL_PREFIX
        };
        write!(f, "{prefix}\n{}", super::render_candidate_reasons(&rows))
    }
}

impl std::error::Error for Exhausted {}

/// Whether sending another prompt after `err` could end differently, for a
/// chain's exhaustion and a lone candidate's failure alike.
fn may_clear(err: &ProviderError) -> bool {
    match Exhausted::of(err) {
        Some(chain) => chain.may_clear(),
        None => candidate_may_clear(err),
    }
}

/// [`may_clear`] for one candidate's error.
///
/// This is not [`is_recoverable`](super::retry::is_recoverable), which names
/// what a retry loop resends: the *same* request. The next prompt sends a
/// different one, so what a loop resends clears, and so does a `200` that was
/// not a completion: rig raises [`ProviderError::Json`] for a body that is not
/// JSON, and [`ProviderError::ProviderResponse`] with a success status for an
/// error envelope a gateway answers with when it accepted a request that then
/// failed. Nothing in a `200` says it cannot clear.
///
/// Anything else -- a `401`, a model that does not exist, a provider-reported
/// fault -- stays terminal.
fn candidate_may_clear(err: &ProviderError) -> bool {
    match err {
        ProviderError::Json(_) => true,
        other if other
            .provider_response_status()
            .is_some_and(|status| status.is_success()) =>
        {
            true
        }
        other => retry::is_recoverable(other),
    }
}

/// Whether `err` is a provider's refusal of the request itself.
fn refused(err: &ProviderError) -> bool {
    err.provider_response_status()
        .is_some_and(|status| matches!(status.as_u16(), 400 | 422))
}

/// Whether `err` is an endpoint's refusal of a request for asking that stale
/// thinking be dropped: one that names the field, the `thinking` it sits in,
/// or the beta it needs. A model that takes no `adaptive` thinking, and a
/// gateway that does not know the beta, refuse it so.
fn refuses_binding(err: &ProviderError) -> bool {
    refused(err)
        && err.provider_response_body().is_some_and(|body| {
            let body = body.to_ascii_lowercase();
            ["block_binding", "thinking", "beta"]
                .iter()
                .any(|term| body.contains(term))
        })
}

/// The report a failure of the chain leaves rig with, which is what the turn
/// reads: its class as a code, so the REPL can tell a whole chain's exhaustion
/// ([`CODE_EXHAUSTED`]) from one endpoint that stayed transiently broken or
/// answered with nothing usable ([`retry::report`]); whether it may clear, as
/// `retryable`; and the chain's own wording of an exhaustion, row by row, as
/// its message.
pub(crate) fn report(error: ProviderError) -> ErrorReport {
    let retryable = may_clear(&error);
    let mut report = match Exhausted::of(&error) {
        Some(exhausted) => {
            let mut report = ErrorReport::from(&error).with_code(CODE_EXHAUSTED);
            report.message = exhausted.to_string();
            report
        }
        None => retry::report(&error),
    };
    report.retryable = retryable;
    report
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
    let PromptError::Report(report) = err else {
        return None;
    };
    if report.code.as_deref() != Some(CODE_EXHAUSTED) {
        return None;
    }
    let message = report.message.as_str();
    let tried = message
        .strip_prefix(EXHAUSTED_PREFIX)
        .or_else(|| message.strip_prefix(EXHAUSTED_TERMINAL_PREFIX))
        .unwrap_or(message);
    Some(ChainExhaustion {
        tried: tried.strip_prefix('\n').unwrap_or(tried),
        recoverable: report.retryable,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rig::completion::message::Origin;
    use rig::completion::{AssistantContent, Usage};

    use super::*;

    /// A candidate that answers however the test says, and counts its calls.
    struct Scripted {
        name: &'static str,
        max_tokens: Option<u64>,
        /// One outcome per call; the last repeats.
        script: Vec<Result<&'static str, ProviderError>>,
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

        fn failing(name: &'static str, error: ProviderError) -> Self {
            Self::new(name, vec![Err(error)])
        }

        fn new(name: &'static str, script: Vec<Result<&'static str, ProviderError>>) -> Self {
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

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::new().with_native_output_tool_composition(self.composes)
        }

        fn completion(
            &self,
            request: CompletionRequest,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + '_>,
        > {
            let nth = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_max_tokens
                .lock()
                .expect("seen")
                .push(request.max_tokens);
            let outcome = match self.script.get(nth).or_else(|| self.script.last()) {
                Some(Ok(text)) => Ok(*text),
                Some(Err(e)) => Err(clone_error(e)),
                None => Err(ProviderError::Provider("script empty".into())),
            };
            // The reply names this candidate as its model, as a provider's
            // wire names the model that answered.
            let origin = Origin::new("mock.script", "mock", self.name);
            Box::pin(async move {
                outcome.map(|text| {
                    CompletionResponse::new(
                        vec![AssistantContent::text(text)],
                        Usage::new(),
                        origin,
                        serde_json::Value::Null,
                    )
                })
            })
        }
    }

    /// `ProviderError` is not `Clone`, and the *variant* matters here -- a
    /// chain of one has to hand its candidate's error back unchanged, which a
    /// fixture that flattened everything to `Provider` could not observe.
    fn clone_error(e: &ProviderError) -> ProviderError {
        match e {
            ProviderError::Response(m) => ProviderError::Response(m.clone()),
            ProviderError::Http(_) => transport_failure(),
            ProviderError::ProviderResponse(response) => {
                ProviderError::ProviderResponse(response.clone())
            }
            other => ProviderError::Provider(other.to_string()),
        }
    }

    /// A transport error with a cause only in its `source()`, as reqwest's.
    #[derive(Debug)]
    struct Transport;
    impl std::fmt::Display for Transport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error sending request")
        }
    }
    impl std::error::Error for Transport {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&Refused)
        }
    }
    #[derive(Debug)]
    struct Refused;
    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Connection refused")
        }
    }
    impl std::error::Error for Refused {}

    fn transport_failure() -> ProviderError {
        ProviderError::Http(Arc::new(rig::http_client::Error::instance(Transport)))
    }

    /// A non-success status, as rig classifies one the HTTP loop handed back:
    /// the provider's response, its status kept.
    fn http_status(code: u16) -> ProviderError {
        ProviderError::from_http_response(
            reqwest::StatusCode::from_u16(code).expect("status"),
            "boom",
        )
    }

    fn request() -> CompletionRequest {
        CompletionRequest::new("hello")
    }

    fn chain(candidates: Vec<Box<dyn Candidate>>) -> FailoverModel {
        FailoverModel::new(candidates, RetryPolicy::default())
    }

    /// What the turn is handed for `err`: the report the chain leaves rig
    /// with, as `handle_prompt_error` reads it.
    fn reported(err: ProviderError) -> PromptError {
        PromptError::Report(report(err))
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

        let err = reported(err);
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

        let err = reported(err);
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
                ProviderError::Response("empty body".into()),
            )),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let label = chain_exhausted_label(&reported(err))
            .expect("reported as a chain failure")
            .tried
            .to_string();
        assert!(label.contains("bedrock") && label.contains("503"), "{label}");
        assert!(
            label.contains("anthropic") && label.contains("empty body"),
            "{label}"
        );
    }

    /// A transport failure in a `tried` row names its cause, as it does on the
    /// retry line, rather than rig's `HttpError:` wrapper.
    #[tokio::test]
    async fn a_transport_failure_is_reported_with_its_cause() {
        let transport = transport_failure();
        let err = chain(vec![
            Box::new(Scripted::failing("head", transport)),
            Box::new(Scripted::failing("next", http_status(503))),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let label = chain_exhausted_label(&reported(err))
            .expect("reported as a chain failure")
            .tried
            .to_string();
        assert!(label.contains("error sending request: Connection refused"), "{label}");
        assert!(!label.contains("HttpError"), "{label}");
    }

    /// A chain of one hands back its candidate's error *as it stands*, so its
    /// report names the class that one endpoint's failure was: transient,
    /// never a chain's exhaustion.
    #[tokio::test]
    async fn a_single_candidate_chain_returns_its_error_unwrapped() {
        let err = chain(vec![Box::new(Scripted::failing("only", http_status(503)))])
            .completion(request())
            .await
            .expect_err("the one candidate failed");

        assert!(
            Exhausted::of(&err).is_none(),
            "a chain of one must not wrap its candidate's error: {err:?}"
        );
        let prompt_err = reported(err);
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
        let composes = |chain: &FailoverModel| chain.capabilities.composes_native_output_with_tools;

        let mixed = chain(vec![
            Box::new(Scripted::ok("a").composing(true)),
            Box::new(Scripted::ok("b").composing(false)),
        ]);
        assert!(!composes(&mixed));

        let homogeneous = chain(vec![
            Box::new(Scripted::ok("a").composing(true)),
            Box::new(Scripted::ok("b").composing(true)),
        ]);
        assert!(
            composes(&homogeneous),
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

    /// #265: after a call that moved, the record holds the candidate that
    /// answered. Through a clone, since the agent's handler table holds one
    /// and what it records has to reach whoever holds the other.
    #[tokio::test]
    async fn the_record_holds_the_candidate_a_move_landed_on() {
        let model = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(503))),
            Box::new(Scripted::ok("anthropic")),
        ]);

        model
            .clone()
            .completion(request())
            .await
            .expect("moves to second");

        assert_eq!(model.take_answered(), [1]);
    }

    /// A head that answers is recorded like any other candidate: whether that
    /// is worth saying is for the reader of the record, not the record.
    #[tokio::test]
    async fn the_record_holds_the_head_when_only_the_head_answered() {
        let model = chain(vec![
            Box::new(Scripted::ok("bedrock")),
            Box::new(Scripted::ok("anthropic")),
        ]);

        model.completion(request()).await.expect("head answers");
        model.completion(request()).await.expect("head answers");

        assert_eq!(model.take_answered(), [0]);
    }

    /// A round is several calls and each starts at the head, so one can be
    /// answered by a fallback and the next by the head: the record is a set,
    /// in chain order, and reading it starts it over.
    #[tokio::test]
    async fn a_mixed_round_records_both_and_reading_drains() {
        let model = chain(vec![
            Box::new(Scripted::new(
                "bedrock",
                vec![Err(http_status(429)), Ok("recovered")],
            )),
            Box::new(Scripted::ok("anthropic")),
        ]);

        model.completion(request()).await.expect("moves to second");
        model.completion(request()).await.expect("head recovered");

        assert_eq!(model.take_answered(), [0, 1]);
        assert!(
            model.take_answered().is_empty(),
            "the next round starts from an empty record"
        );
    }

    /// A call that failed on every candidate produced no output, so nothing
    /// is attributed to anyone.
    #[tokio::test]
    async fn an_exhausted_chain_records_nothing() {
        let model = chain(vec![
            Box::new(Scripted::failing("bedrock", http_status(503))),
            Box::new(Scripted::failing("anthropic", http_status(503))),
        ]);

        model
            .completion(request())
            .await
            .expect_err("every candidate failed");

        assert!(model.take_answered().is_empty());
    }

    /// Streaming reaches no candidate at all: outrig never streams, and a chain
    /// refuses rather than fanning a stream out.
    #[tokio::test]
    async fn streaming_is_refused_rather_than_fanned_out() {
        let candidate = Scripted::ok("only");
        let calls = Arc::clone(&candidate.calls);
        let reply = chain(vec![Box::new(candidate)])
            .serve(
                EffectKind::Completion {
                    request: Arc::new(request()),
                    stream: true,
                },
                Dispatch::new(rig::effect::EffectId::from_raw(0), true),
            )
            .await;
        assert!(matches!(reply, Reply::Outcome(Err(_))), "a stream is refused");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
