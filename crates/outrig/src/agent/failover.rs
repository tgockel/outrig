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
//! * **The chain is the agent's model.** rig's runner calls its model through
//!   a handler it dispatches completions to, and lets the host register that
//!   handler ([`rig::agent::AgentBuilder::over_handlers`]). `FailoverModel` is
//!   that handler ([`Serve`]): it answers each completion the runner
//!   dispatches with the reply of whichever candidate answered, as that
//!   candidate's wire decoded it, so the reply keeps the provider and model it
//!   came from -- which is what lets a later call replay the model's thinking.
//!   A chain built as a rig model instead would restamp every reply as the
//!   chain's own.
//! * **The candidates are heterogeneous.** An alias may span an OpenAI row and
//!   an Anthropic one. rig erases a model's wire and transport behind a
//!   [`DynModel`](rig::DynModel), and [`Candidate`] is the small trait that
//!   stands in front of that with what a chain rewrites per candidate.
//! * **The budget has to be shared.** Three candidates each spending the full
//!   `retry-budget-secs` against a total outage is a worst case worse than no
//!   failover at all, so the chain arms one [`ChainDeadline`] per call and
//!   every candidate's retry loop below it observes the same bound.
//! * **Each candidate has its own window.** The chain is the only place a
//!   call's view is assembled. It asks the store for one against each
//!   candidate's [`Budget`], the head's included -- a smaller model is a
//!   smaller window, and what fit one may not fit another. A candidate whose
//!   window cannot hold the call's latest turn is passed over for the next,
//!   and only when none can does the call end ([`unfit`]).
//! * **Each candidate has its own count.** The chain holds a [`Calibration`]
//!   for each candidate, learned from the usage that candidate reported for the
//!   view it was sent, and scales that candidate's next view by it.
//! * **Each candidate reads what the others wrote.** One model's reply joins
//!   the conversation every model is sent, so each candidate's view is
//!   assembled for its provider's protocol as well as its window: reasoning
//!   Anthropic cannot take back is left out of a call to it ([`Wire`]).
//! * **A failure must not disappear.** Each candidate's reason for being
//!   abandoned is kept, and exhausting the chain reports all of them together
//!   rather than only the last.
//! * **A refusal of what a call carried is not the last word.** A candidate
//!   that ends a reply for its content -- `content_filter`, `refusal`
//!   -- past its retries is sent the call once more, with the results of the
//!   turn it answers replaced by a notice: the one thing that call carried no
//!   earlier one had. Answered, every later call is sent the notice too, so a
//!   result the provider will not read cannot refuse the rest of the session;
//!   refused again, nothing stays withheld, and the call moves on.
//! * **Neither is a refusal of `drop_block`.** rig asks Anthropic's API to
//!   drop thinking a change to the conversation made stale, as a backstop to
//!   the view, which leaves it out itself -- for a model its catalog says
//!   binds its thinking to the conversation, under the beta that takes the
//!   request. An endpoint at a custom `base-url` that refuses the request for
//!   asking -- a gateway that does not know the beta -- is sent the call again
//!   through the same model built not to ask, and every later call to it goes
//!   without ([`Candidate::unbind`]).
//!
//! Copied from `outrig-cli`'s `llm/failover.rs`, which keeps its own. Besides
//! the view, what changed in coming across is how the chain's state gets out.
//! The CLI recovers it by prefix-matching the error string a chain's exhaustion
//! is rendered into. Here a move is a `model.failover` event as it happens, the
//! candidate that answered is the [`CallSlot`]'s to say
//! ([`CallSlot::answered_model`]), and a failure leaves the chain as a report
//! that names its class ([`report`]). Nothing parses an error.
//!
//! [`RetryingHttpClient`]: super::retry::RetryingHttpClient
//! [`RetryingModel`]: super::retry::RetryingModel
//! [`ChainDeadline`]: super::retry::ChainDeadline
//! [`retry`]: super::retry
//! [`Wire`]: super::budget::Wire
//! [`Calibration`]: super::budget::Calibration

// rig's `ProviderError` is what each candidate hands back by value, and what
// the chain carries from one to the next; boxing it in OutRig's own functions
// would only move the size.
#![allow(clippy::result_large_err)]

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rig::completion::{
    CompletionRequest, CompletionResponse, Message, ModelRef, ProviderCapabilities,
};
use rig::effect::{Answered, EffectKind, FamilyDescriptor, HandlerDescriptor, family, model_key};
use rig::error::{ErrorKind, ErrorReport, ProviderError};
use rig::serve::{Dispatch, Reply, Serve};

use super::budget::{Budget, Calibration, Wire};
use super::history::{Adjacent, History, Refused, TooLarge, Why};
use super::ledger::CallSlot;
use super::retry::{BudgetSpent, RetryPolicy, RetryingModel, candidate_failure_label};
use crate::harness::event::{CallId, Payload};
use rig::providers::openai::extension::OpenAiOptions;

/// The label the chain is registered under in the agent's handler table, which
/// is what rig names the agent's model.
pub(crate) const CHAIN_LABEL: &str = "outrig";

/// The code a report carries when every candidate was abandoned, and the
/// report's `retryable` says whether any reason could clear.
pub(crate) const CODE_EXHAUSTED: &str = "outrig.exhausted";
/// The code a report carries when the call was not made because no candidate's
/// window could hold its latest turn; its message says which turn.
pub(crate) const CODE_UNFIT: &str = "outrig.unfit";
/// The code a report carries when the session's spent token budget refused the
/// request.
pub(crate) const CODE_BUDGET_SPENT: &str = "outrig.budget-spent";

/// One candidate of a chain, behind an object-safe interface: run one
/// completion, say what the provider composes, and carry what must be
/// rewritten per candidate.
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

    /// [`Self::budget`], to change, for a test that holds a chain it built to
    /// a window it could not know to configure.
    #[cfg(test)]
    fn budget_mut(&mut self) -> &mut Budget;

    /// The concrete `[models.<name>]` row, for the per-candidate report and the
    /// move announcement. Never the alias's name.
    fn model_name(&self) -> &str {
        &self.budget().model
    }

    /// What this candidate's provider composes: whether native structured
    /// output composes with tools, and whether a forced tool choice is refused.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Stop asking the API to drop thinking a change to the conversation made
    /// stale (`drop_block`), because this candidate's endpoint refused a
    /// request that asked: whether it was asking. A call leaves such thinking
    /// out itself either way ([`History`]), so only the backstop goes.
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
    model_identifier: String,
    budget: Budget,
    /// For an Anthropic candidate, the same model built not to ask for
    /// `drop_block`, which its calls go through once its endpoint has refused
    /// that.
    binding: Option<Binding>,
}

impl ModelCandidate {
    pub(crate) fn new(
        model: RetryingModel,
        model_identifier: impl Into<String>,
        budget: Budget,
        binding: Option<Binding>,
    ) -> Self {
        Self {
            model,
            model_identifier: model_identifier.into(),
            budget,
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
    fn model_identifier(&self) -> &str {
        &self.model_identifier
    }

    fn budget(&self) -> &Budget {
        &self.budget
    }

    #[cfg(test)]
    fn budget_mut(&mut self) -> &mut Budget {
        &mut self.budget
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

/// How a chain assembles a call's conversation for each candidate it sends the
/// call to.
///
/// [`History`] is the one there is. The trait is what lets the chain's own
/// tests stand in for the store.
pub(crate) trait View: Send + Sync {
    /// What call `call` to a candidate held to `budget` is sent, or why the
    /// prompt does not fit. With `refused`, each result of the turn it names
    /// is sent as a notice in its place, this once.
    fn view(
        &self,
        budget: &Budget,
        call: CallId,
        refused: Option<&Refused>,
    ) -> Result<Viewed, TooLarge>;

    /// Send every later call the notice `refused` names in place of each of
    /// its turn's results.
    fn withhold(&self, refused: Refused);
}

/// What a call to one candidate is sent of the conversation.
pub(crate) struct Viewed {
    /// The messages, oldest first, ending with the call's prompt.
    messages: Vec<Message>,
    /// The first place they put one role after itself, if any.
    adjacent: Option<Adjacent>,
    /// What the rule estimates the whole request at, before the candidate's
    /// scale: the number its provider's report of the request is compared with.
    estimate: u64,
    /// The reply ceiling the call carries: the candidate's, unless the scale
    /// would have refused the call's prompt and the view lowered it to carry
    /// it ([`Budget::making_room_for`]).
    max_tokens: Option<u32>,
    /// The turn whose results the call answers, or `None` when its prompt is
    /// a round's opening.
    latest: Option<usize>,
}

/// Recorded as a `model.call` manifest naming the candidate's budget, one for
/// each candidate the call's view is assembled for, and one more for a call
/// sent again without what was refused.
impl View for History {
    fn view(
        &self,
        budget: &Budget,
        call: CallId,
        refused: Option<&Refused>,
    ) -> Result<Viewed, TooLarge> {
        match refused {
            Some(refused) => self.assemble_without(budget, call, refused),
            None => self.assemble(budget, call),
        }
        .map(|(messages, manifest)| Viewed {
            messages,
            adjacent: manifest.adjacent.first().copied(),
            estimate: manifest.estimate,
            max_tokens: manifest.budget.max_tokens,
            latest: manifest
                .carried
                .iter()
                .find(|&&(_, why)| why == Why::Latest)
                .map(|&(turn, _)| turn),
        })
    }

    fn withhold(&self, refused: Refused) {
        History::withhold(self, refused);
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
/// candidate. A `401` is not retryable -- but in a chain it is a reason to try
/// the next candidate, whose key may be fine. So is a call whose latest turn
/// does not fit a candidate's window: that candidate is passed over for the
/// call, with the reason. What must not happen is a failure disappearing, so
/// every reason is kept and reported together on exhaustion -- unless every
/// one is a window too small, which is no failure of any model's but the call
/// not being made ([`unfit`]). The exception is a request the session's spent
/// token budget refused ([`BudgetSpent`]), which ends the call: every
/// candidate would be refused the same.
///
/// A candidate that refused the call for what it carried is first sent it once
/// more without the results of the turn it answers ([`Refused`]), and only
/// that failing too moves the call on.
///
/// Every agent's model is one of these. A chain of one returns its candidate's
/// error as it stands, and arms a deadline whose bound reproduces what the
/// per-attempt budget already gave.
///
/// `Clone` so the agent and the chain's builder can both hold it: the agent's
/// handler table owns one, and what the round reads of the chain -- its slot,
/// its budgets -- is read through another. Every field shares rather than
/// rebuilds: the candidates behind an `Arc`, since a boxed trait object cannot
/// be cloned, and the policy's own clone deliberately shares the chain
/// deadline.
#[derive(Clone)]
pub(crate) struct FailoverModel {
    candidates: Arc<Vec<Box<dyn Candidate>>>,
    /// What each candidate's provider has been seen to count, in the
    /// candidates' order.
    calibrations: Arc<[Calibration]>,
    policy: RetryPolicy,
    /// Reduced across the chain, computed once at build time: see
    /// [`FailoverModel::new`].
    capabilities: ProviderCapabilities,
    /// Where the chain's calls and attempts are named, and each move
    /// recorded.
    slot: CallSlot,
    /// Where each call's view comes from.
    view: Arc<dyn View>,
}

impl FailoverModel {
    /// Build a chain over `candidates`, in preference order, naming its calls
    /// and recording each move through `slot`, and taking each call's view
    /// from `view`.
    ///
    /// `policy` is the one whose [`chain_deadline`] every candidate below
    /// already holds a handle to; arming it here is what bounds them all.
    ///
    /// [`chain_deadline`]: super::retry::RetryPolicy::chain_deadline
    pub(crate) fn new(
        candidates: Vec<Box<dyn Candidate>>,
        policy: RetryPolicy,
        slot: CallSlot,
        view: Arc<dyn View>,
    ) -> Self {
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
            calibrations: candidates.iter().map(|_| Calibration::default()).collect(),
            candidates: Arc::new(candidates),
            policy,
            capabilities,
            slot,
            view,
        }
    }

    /// Where the chain's calls and attempts are named.
    pub(crate) fn slot(&self) -> &CallSlot {
        &self.slot
    }

    /// What a call to each candidate may carry, in the chain's order: the
    /// head's first.
    pub(crate) fn budgets(&self) -> impl Iterator<Item = &Budget> {
        self.candidates.iter().map(|candidate| candidate.budget())
    }

    /// What a call to the candidate at `index` is held to now: its budget at
    /// the scale its provider's reports have set.
    fn held_to(&self, index: usize) -> Budget {
        self.candidates[index]
            .budget()
            .scaled(self.calibrations[index].get())
    }

    /// The `[models.<name>]` row every call is tried against first.
    pub(crate) fn head_name(&self) -> &str {
        self.candidates[0].model_name()
    }

    /// What a call to the head is held to now. A round opens against it, so the
    /// count of turns the opening says its first call leaves out is the count
    /// that call leaves out.
    pub(crate) fn head_budget(&self) -> Budget {
        self.held_to(0)
    }

    /// Hold calls to the head to a `window` of tokens in place of the one it
    /// was built with. Only while no call is in flight.
    #[cfg(test)]
    pub(crate) fn narrow_head(&mut self, window: u32) {
        let candidates = Arc::get_mut(&mut self.candidates).expect("no call is in flight");
        candidates[0].budget_mut().window = window;
    }

    /// Say that `from` was abandoned for `error` and the call moves to `to`.
    ///
    /// Announced once per move, beside the retry lines. An alias already widens
    /// what a config typo can silently do; a chain that moves mid-round means
    /// one round can be half one model's work, which is the stronger version of
    /// the same hazard and wants the same mitigation.
    fn announce(&self, from: &str, to: &str, error: &ProviderError, call: CallId) {
        let error = candidate_failure_label(error);
        tracing::warn!("model {from} failed ({error}); trying {to}");
        self.slot.events().emit(Payload::ModelFailover {
            from: from.to_string(),
            to: to.to_string(),
            error,
            call_id: call,
            attempt_id: self.slot.last_attempt(),
        });
    }
}

/// The failure a chain returns when every candidate has been abandoned, boxed
/// into [`ProviderError::Request`] the way [`TooLarge`] is, so the chain can
/// [read it back](Self::of) into the report it hands rig instead of parsing
/// its text.
///
/// Rendered as one line per candidate, mirroring the shape resolution uses for
/// an alias with no selectable candidate: three candidates failing for three
/// different reasons is exactly the case a single-line error wastes an
/// afternoon on. It says whether any reason could clear without a change to the
/// config -- one vendor rate-limiting while another's key is revoked is still
/// worth waiting out, because the rate limit is the one that can lift.
#[derive(Debug)]
pub(crate) struct Exhausted {
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
    pub(crate) fn of(err: &ProviderError) -> Option<&Self> {
        match err {
            ProviderError::Request(inner) => (**inner).downcast_ref(),
            _ => None,
        }
    }

    /// Whether at least one candidate failed for a reason a later round could get past.
    fn may_clear(&self) -> bool {
        self.abandoned.iter().any(|a| candidate_may_clear(&a.error))
    }

    /// The reason the call was not made, when every candidate was passed over
    /// for the size of its latest turn: see [`unfit`].
    fn unfit(&self) -> Option<String> {
        let rows = self
            .abandoned
            .iter()
            .map(|a| Some((a.model_name.as_str(), too_large(&a.error)?.to_string())))
            .collect::<Option<Vec<_>>>()?;
        Some(format!(
            "no model candidate has room for the call; tried:\n{}",
            super::resolve::render_candidate_reasons(&rows)
        ))
    }
}

impl fmt::Display for Exhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: Vec<_> = self
            .abandoned
            .iter()
            .map(|a| (a.model_name.as_str(), candidate_failure_label(&a.error)))
            .collect();
        let failed = if self.may_clear() {
            "every model candidate failed"
        } else {
            "every model candidate failed terminally"
        };
        write!(
            f,
            "{failed}; tried:\n{}",
            super::resolve::render_candidate_reasons(&rows)
        )
    }
}

impl std::error::Error for Exhausted {}

/// Whether sending another message after `err` could end differently, for a
/// chain's exhaustion and a lone model's failure alike.
///
/// Asked of every candidate when `err` is a chain's, and a chain's is worth
/// another round when any one is: a rate limit that outlasted the budget may
/// lift, and a key every provider refuses will not.
pub(crate) fn may_clear(err: &ProviderError) -> bool {
    match Exhausted::of(err) {
        Some(chain) => chain.may_clear(),
        None => candidate_may_clear(err),
    }
}

/// [`may_clear`] for one candidate's error.
///
/// This is not [`is_recoverable`](super::retry::is_recoverable), which names
/// what a retry loop resends: the *same* request. The next round sends a
/// different one, so what a loop resends clears, and so do two failures no
/// loop would repeat:
///
/// - A call not sent because its latest turn did not fit the candidate's
///   window ([`TooLarge`]). Resending it fails again, but the turn stays in
///   `runtime.history` and the next round's view leaves it out.
/// - A `200` that was not a completion. rig raises [`ProviderError::Json`]
///   for a body that is not JSON, or, for an Anthropic-style provider, not a
///   message; and [`ProviderError::ProviderResponse`] with a success status
///   for an OpenAI-style provider's body that is not a completion, such as the
///   error envelope a gateway answers with when it accepted a request that
///   then failed from an overload or a timeout. Nothing in a `200` says it
///   cannot clear, and a later call may not come back that way. A body that
///   names a permanent cause, or a wrong `base-url` that answers `200` with
///   something else, reads the same; the reason carries the error or the body,
///   and the advice is what every failed round gave.
///
/// Anything else -- a `401`, a model that does not exist, a provider-reported
/// fault -- stays terminal.
fn candidate_may_clear(err: &ProviderError) -> bool {
    match err {
        ProviderError::Json(_) => true,
        ProviderError::Request(inner) => (**inner).is::<TooLarge>(),
        other
            if other
                .provider_response_status()
                .is_some_and(|status| status.is_success()) =>
        {
            true
        }
        other => super::retry::is_recoverable(other),
    }
}

/// Why a call was not made, when it was not because no candidate's window could
/// hold its latest turn: the reason the round ends there, keeping the turn,
/// where any other failure of a call is an error.
///
/// A lone model's reason is its own, as [`TooLarge`] words it. A chain's is
/// every candidate's, in the chain's order, when each was passed over for the
/// size of the turn. A chain that sent the call to one candidate, and had it
/// fail, is not this: that is a failure, whatever the others' windows held.
pub(crate) fn unfit(err: &ProviderError) -> Option<String> {
    match Exhausted::of(err) {
        Some(chain) => chain.unfit(),
        None => too_large(err).map(ToString::to_string),
    }
}

/// The call that was not sent to a candidate because its latest turn did not
/// fit, when `err` is that.
fn too_large(err: &ProviderError) -> Option<&TooLarge> {
    match err {
        ProviderError::Request(inner) => (**inner).downcast_ref(),
        _ => None,
    }
}

/// Whether `err` is a provider's refusal of the request itself, of the kind a
/// conversation that put one role after itself earns.
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

/// `history`, with the messages after the system prompt replaced by `view`.
///
/// rig puts an agent's preamble at the head of the request's history as a
/// system message, so a view assembled from the store goes after it rather
/// than in place of it. A view with nothing in it leaves the system prompt
/// alone, which is how a call's requests are built without rig's prompt; a
/// history with no system prompt to leave is kept as it was.
fn after_system(history: &[Arc<Message>], view: Vec<Message>) -> Vec<Arc<Message>> {
    let system = history
        .iter()
        .take_while(|message| matches!(&***message, Message::System { .. }))
        .cloned();
    let replaced: Vec<Arc<Message>> = system.chain(view.into_iter().map(Arc::new)).collect();
    if replaced.is_empty() {
        history.to_vec()
    } else {
        replaced
    }
}

/// The report a failure of the chain leaves rig with, which is what the round
/// reads: its class as a code, so the round can tell a call not made
/// ([`CODE_UNFIT`]) or refused for the spent budget ([`CODE_BUDGET_SPENT`])
/// from one that failed; whether it may clear, as `retryable`; and the
/// chain's own wording of an exhaustion, row by row, as its message.
fn report(error: ProviderError) -> ErrorReport {
    if BudgetSpent::of(&error) {
        return ErrorReport::new(ErrorKind::Request, error.to_string())
            .with_code(CODE_BUDGET_SPENT)
            .with_retryable(false);
    }
    if let Some(reason) = unfit(&error) {
        return ErrorReport::new(ErrorKind::Request, reason)
            .with_code(CODE_UNFIT)
            .with_retryable(true);
    }
    let retryable = may_clear(&error);
    let (message, code) = match Exhausted::of(&error) {
        Some(exhausted) => (exhausted.to_string(), Some(CODE_EXHAUSTED.to_string())),
        None => (candidate_failure_label(&error), None),
    };
    let mut report = ErrorReport::from(&error);
    report.message = message;
    report.retryable = retryable;
    if code.is_some() {
        report.code = code;
    }
    report
}

/// Whether `report` says the session's spent token budget refused the call.
pub(crate) fn budget_spent_report(report: &ErrorReport) -> bool {
    report.code.as_deref() == Some(CODE_BUDGET_SPENT)
}

/// Why the call was not made, when `report` says no candidate had room for it.
pub(crate) fn unfit_report(report: &ErrorReport) -> Option<&str> {
    (report.code.as_deref() == Some(CODE_UNFIT)).then_some(report.message.as_str())
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
                "streaming is not supported through a model chain",
            )),
            other => Err(ErrorReport::new(
                ErrorKind::HandlerUnavailable,
                format!("a model chain serves completions, not {}", other.name()),
            )),
        };
        Reply::Outcome(outcome)
    }
}

impl FailoverModel {
    /// One completion: each candidate in turn, until one answers.
    async fn completion(
        &self,
        mut template: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        // Armed per call, not per round or per session: a round is a sequence
        // of these with Python run between them, and each one gets a whole
        // budget to find a working candidate.
        self.policy.chain_deadline.arm(self.policy.budget);
        let call = self.slot.begin_call();

        let mut abandoned: Vec<Abandoned> = Vec::new();
        // What every request the call sends is built from. What rig put after
        // the system prompt is the prompt alone, the round's hook having
        // patched the rest out, and each view ends with it, so it is left out
        // here: what is cloned for each request is the system prompt and the
        // tool, never the prompt or a view.
        template.chat_history = after_system(&template.chat_history, Vec::new());
        // Every call starts at the head. The list is a preference order, and
        // sticking to candidate two for the rest of a session because of one
        // rate-limit window would silently downgrade the user's choice. The
        // cost is one attempt against a still-dead endpoint per call, which the
        // short pre-first-byte connect bound makes cheap.
        for (index, candidate) in self.candidates.iter().enumerate() {
            let budget = self.held_to(index);
            // Set once the candidate has refused the call for what it carried,
            // for the one time it is sent again.
            let mut withholding: Option<Refused> = None;
            let error = loop {
                let mut attempt = template.clone();
                // Every candidate is sent a view assembled for its own window,
                // the head's too, which may leave out more -- or cannot hold
                // the call's latest turn at all -- and for its own count of
                // tokens.
                let viewed = self.view.view(&budget, call, withholding.as_ref());
                let max_tokens = viewed
                    .as_ref()
                    .map_or(budget.max_tokens, |viewed| viewed.max_tokens);
                self.slot.candidate(
                    candidate.model_name(),
                    candidate.model_identifier(),
                    max_tokens,
                );
                if withholding.is_some() {
                    // Its retries were the first send's.
                    self.slot.send_once();
                }
                let viewed = viewed.map(|viewed| {
                    attempt.chat_history = after_system(&attempt.chat_history, viewed.messages);
                    (viewed.adjacent, viewed.estimate, viewed.latest)
                });
                // Both rewrites are per candidate. `max_tokens` is the
                // load-bearing one: the agent carries none of its own, so
                // without this no ceiling would be sent. `model` arrives `None`
                // on the agent path and each candidate's rig model already
                // carries its own identifier, so setting it is belt-and-braces
                // against a future rig that populates the field -- correct,
                // and free.
                attempt.model = Some(candidate.model_identifier().to_string());
                attempt.max_tokens = max_tokens.map(u64::from);
                // Responses keeps what it is sent unless told not to, and the
                // conversation is OutRig's to carry.
                if budget.wire == Wire::Responses {
                    attempt = attempt.provider_option(OpenAiOptions::new().store(false));
                }

                let (adjacent, estimate, latest) = match viewed {
                    // Not made: the call cannot fit this candidate. Sending it
                    // again fails the same way, which is what a request error
                    // is to `is_recoverable`; the next round's view leaves the
                    // turn out, which `candidate_may_clear` counts.
                    Err(too_large) => break ProviderError::request(too_large),
                    Ok(viewed) => viewed,
                };
                match candidate.completion(attempt).await {
                    Ok(response) => {
                        if let Some(withheld) = withholding {
                            self.view.withhold(withheld);
                        }
                        // Against the estimate before its scale, or each call
                        // would be measured against a correction of the last.
                        self.calibrations[index]
                            .learn(budget.wire.reported_input(&response.usage), estimate);
                        // As the candidate's wire decoded it, its provider and
                        // model kept: the slot says which row answered.
                        return Ok(response);
                    }
                    // The session's spent budget would refuse every candidate
                    // the same.
                    Err(spent) if BudgetSpent::of(&spent) => return Err(spent),
                    Err(error) => {
                        // Refused for asking that stale thinking be dropped:
                        // sent again without asking, as every later call to it
                        // is.
                        if refuses_binding(&error) && candidate.unbind() {
                            tracing::warn!(
                                "model {} refused the request that asked for drop_block \
                                 ({error}); sending it and every later call without asking, and \
                                 leaving out stale thinking as before",
                                candidate.model_name()
                            );
                            continue;
                        }
                        // What the round says of a refusal is about the view
                        // that was refused, which only this loop has.
                        if let Some(adjacent) = adjacent
                            && refused(&error)
                        {
                            self.slot.refused(&budget, adjacent);
                        }
                        // Refused for what it carried, the first time, with
                        // results of its own to leave out: a round's opening
                        // has none.
                        if withholding.is_none()
                            && let Some(turn) = latest
                            && let Some(finish_reason) = self.slot.filtered()
                        {
                            withholding = Some(Refused {
                                turn,
                                finish_reason,
                            });
                            continue;
                        }
                        break error;
                    }
                }
            };
            if let Some(next) = self.candidates.get(index + 1) {
                self.announce(candidate.model_name(), next.model_name(), &error, call);
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
            Err(many) => Err(Exhausted::into_error(many)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rig::completion::message::Origin;
    use rig::completion::{AssistantContent, Usage};

    use super::*;
    use crate::agent::budget::Scale;
    use crate::agent::history::Role;
    use crate::agent::ledger::{Ledger, Refusal};
    use crate::agent::mock_http::transport_failure;

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
        script: Vec<Result<&'static str, ProviderError>>,
        calls: Arc<AtomicUsize>,
        /// Every request this candidate was actually sent.
        ///
        /// Behind an `Arc` for the same reason `calls` is: a test clones the
        /// handle before the fixture is boxed into the chain, which is what
        /// lets it inspect a candidate it no longer owns.
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
        composes: bool,
        /// The input tokens each answer reports having cost.
        input_tokens: u64,
        /// Where each failure is said to be a refusal of what the request
        /// carried, when this candidate's are.
        filtering: Option<CallSlot>,
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
                budget: budget(name, None),
                script,
                calls: Arc::new(AtomicUsize::new(0)),
                seen: Arc::default(),
                composes: true,
                input_tokens: 0,
                filtering: None,
            }
        }

        /// Each failure of this candidate is a refusal of what the request
        /// carried, said through `slot`, as its retry layer would say it.
        fn filtering(mut self, slot: &CallSlot) -> Self {
            self.filtering = Some(slot.clone());
            self
        }

        /// This candidate's answers report `input_tokens` as the request's cost.
        fn reporting(mut self, input_tokens: u64) -> Self {
            self.input_tokens = input_tokens;
            self
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

        fn budget_mut(&mut self) -> &mut Budget {
            &mut self.budget
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
            self.seen.lock().expect("seen").push(request);
            let outcome = match self.script.get(nth).or_else(|| self.script.last()) {
                Some(Ok(text)) => Ok(*text),
                Some(Err(e)) => Err(clone_error(e)),
                None => Err(ProviderError::Provider("script empty".into())),
            };
            if let (Err(_), Some(slot)) = (&outcome, &self.filtering) {
                slot.filter("content_filter");
            }
            let input_tokens = self.input_tokens;
            // The reply names this candidate as its model, as a provider's
            // wire names the model that answered.
            let origin = Origin::new("mock.script", "mock", self.budget.model.clone());
            Box::pin(async move {
                outcome.map(|text| {
                    CompletionResponse::new(
                        vec![AssistantContent::text(text)],
                        Usage::new().input_tokens(input_tokens),
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
            e if BudgetSpent::of(e) => spent(),
            ProviderError::Response(m) => ProviderError::Response(m.clone()),
            ProviderError::Http(_) => transport_failure(),
            ProviderError::Json(_) => undecodable(),
            ProviderError::ProviderResponse(response) => {
                ProviderError::ProviderResponse(response.clone())
            }
            other => ProviderError::Provider(other.to_string()),
        }
    }

    /// A response that did not decode, as rig raises one.
    fn undecodable() -> ProviderError {
        ProviderError::Json(Arc::new(serde_json::from_str::<()>("{").unwrap_err()))
    }

    /// A non-success status, as rig classifies one the HTTP loop handed back:
    /// the provider's response, its status kept.
    fn http_status(code: u16) -> ProviderError {
        ProviderError::from_http_response(
            reqwest::StatusCode::from_u16(code).expect("status"),
            "boom",
        )
    }

    /// What a request gets once the session's token budget is spent.
    fn spent() -> ProviderError {
        BudgetSpent::error()
    }

    /// A request as rig builds one for an agent with a preamble: the system
    /// prompt first, then the conversation.
    fn request() -> CompletionRequest {
        CompletionRequest::new("hello").preamble("preamble")
    }

    /// What every view a store stand-in assembles is estimated at.
    const ESTIMATE: u64 = 1_000;

    /// A store stand-in: the view for a budget names its model, and the
    /// models it was asked for are kept, with the scale each was held to.
    /// `too_large_for` names the candidates whose window the call's latest turn
    /// does not fit, and `adjacent_for` one whose view puts the model's reply
    /// after its own.
    #[derive(Default)]
    struct Views {
        asked: Mutex<Vec<(String, Scale)>>,
        /// The ceiling every view lowers the call to, if it does.
        lowered_to: Option<u32>,
        too_large_for: Vec<&'static str>,
        adjacent_for: Vec<&'static str>,
        /// The turn every view answers the results of, or `None` for a
        /// round's opening.
        latest: Option<usize>,
        /// The models each view without a turn's results was asked for, in
        /// the order asked.
        without: Mutex<Vec<String>>,
        /// What every later view is to leave out, in the order kept.
        withheld: Mutex<Vec<Refused>>,
    }

    impl Views {
        /// The model and scale of each view asked for, in the order asked.
        fn asked(&self) -> Vec<(String, Scale)> {
            self.asked.lock().expect("asked").clone()
        }

        /// The models of each view asked for, in the order asked.
        fn models(&self) -> Vec<String> {
            self.asked().into_iter().map(|(model, _)| model).collect()
        }

        /// Views whose calls answer the results of turn `turn`.
        fn answering(turn: usize) -> Self {
            Self {
                latest: Some(turn),
                ..Self::default()
            }
        }

        fn too_large_for(names: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                too_large_for: names.into_iter().collect(),
                ..Self::default()
            }
        }

        fn adjacent_for(names: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                adjacent_for: names.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl View for Views {
        fn view(
            &self,
            budget: &Budget,
            _call: CallId,
            refused: Option<&Refused>,
        ) -> Result<Viewed, TooLarge> {
            self.asked
                .lock()
                .expect("asked")
                .push((budget.model.clone(), budget.scale));
            if refused.is_some() {
                self.without
                    .lock()
                    .expect("without")
                    .push(budget.model.clone());
            }
            let named = |names: &[&str]| names.contains(&budget.model.as_str());
            if named(&self.too_large_for) {
                return Err(TooLarge {
                    turn: Some(3),
                    round: 1,
                    estimate: 9_000,
                    budget: budget.clone(),
                });
            }
            let adjacent = named(&self.adjacent_for).then_some(Adjacent {
                turn: Some(2),
                role: Role::Assistant,
            });
            Ok(Viewed {
                messages: vec![Message::user(format!("the view for {}", budget.model))],
                adjacent,
                estimate: ESTIMATE,
                max_tokens: self.lowered_to.or(budget.max_tokens),
                latest: self.latest,
            })
        }

        fn withhold(&self, refused: Refused) {
            self.withheld.lock().expect("withheld").push(refused);
        }
    }

    fn chain(candidates: Vec<Box<dyn Candidate>>) -> FailoverModel {
        chain_over(candidates, Arc::new(Views::default()), CallSlot::default())
    }

    fn chain_over(
        candidates: Vec<Box<dyn Candidate>>,
        view: Arc<dyn View>,
        slot: CallSlot,
    ) -> FailoverModel {
        FailoverModel::new(candidates, RetryPolicy::default(), slot, view)
    }

    /// The text of the exhaustion a chain's failure is.
    fn exhaustion(err: ProviderError) -> String {
        match Exhausted::of(&err) {
            Some(chain) => chain.to_string(),
            None => panic!("not a chain's exhaustion: {err:?}"),
        }
    }

    /// What a chain of `candidates`, each failing as given, returns.
    async fn failed_with(
        failures: impl IntoIterator<Item = (&'static str, ProviderError)>,
    ) -> ProviderError {
        let candidates = failures
            .into_iter()
            .map(|(name, error)| Box::new(Scripted::failing(name, error)) as Box<dyn Candidate>)
            .collect();
        chain(candidates)
            .completion(request())
            .await
            .expect_err("every candidate failed")
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
            reply.origin.model, "anthropic",
            "the reply is the candidate's own, naming the model that gave it"
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

    /// A transport failure in a `tried` row names its cause, as it does on the
    /// retry line, rather than rig's `Http client error:` wrapper.
    #[tokio::test]
    async fn a_transport_failure_is_reported_with_its_cause() {
        let err = chain(vec![
            Box::new(Scripted::failing("head", transport_failure())),
            Box::new(Scripted::failing("next", http_status(503))),
        ])
        .completion(request())
        .await
        .expect_err("every candidate failed");

        let text = exhaustion(err);
        assert!(
            text.contains("error sending request: Connection refused"),
            "{text}"
        );
        assert!(!text.contains("Http client error"), "{text}");
    }

    /// What the round reads of a failure is the typed exhaustion, never its
    /// text: only a chain of more than one returns it, and a lone candidate's
    /// error reaches the round as it stands.
    #[tokio::test]
    async fn only_a_real_chain_returns_an_exhaustion() {
        let chained = failed_with([
            ("bedrock", http_status(401)),
            ("anthropic", http_status(401)),
        ])
        .await;
        assert!(Exhausted::of(&chained).is_some());

        let lone = failed_with([("bedrock", http_status(401))]).await;
        assert!(Exhausted::of(&lone).is_none());
        assert!(
            matches!(lone, ProviderError::ProviderResponse(_)),
            "{lone:?}"
        );
        assert!(Exhausted::of(&ProviderError::request("other")).is_none());
    }

    /// Whether a resend could help is the chain's verdict for a chain and the
    /// candidate's own for a lone model: a rate limit may lift, a refused key
    /// will not, and one that may is enough for a chain.
    #[tokio::test]
    async fn a_resend_is_worth_it_when_any_failure_could_clear() {
        let cases = [
            (vec![401, 401], false),
            (vec![401, 403], false),
            (vec![401, 503], true),
            (vec![429, 401], true),
            (vec![401], false),
            (vec![429], true),
            (vec![503], true),
        ];
        for (statuses, clears) in cases {
            let names = ["bedrock", "anthropic", "vertex"];
            let err = failed_with(
                statuses
                    .iter()
                    .zip(names)
                    .map(|(status, name)| (name, http_status(*status))),
            )
            .await;
            assert_eq!(may_clear(&err), clears, "{statuses:?}: {err}");
        }
    }

    /// The next round sends a different request, so a failure no loop retries
    /// can still be worth a message: a response that did not decode, an error
    /// envelope a gateway sent with a `200`, and a call a fallback's window
    /// could not hold, whose turn the next view leaves out. A reason with
    /// nothing to get past it stays terminal.
    #[tokio::test]
    async fn a_failure_the_next_round_gets_past_is_worth_a_resend() {
        let lone = failed_with([("bedrock", undecodable())]).await;
        assert!(
            may_clear(&lone),
            "a lone model's undecodable response: {lone}"
        );
        let chained =
            failed_with([("bedrock", http_status(401)), ("anthropic", undecodable())]).await;
        assert!(may_clear(&chained), "{chained}");

        let envelope = || {
            ProviderError::from_http_response(
                reqwest::StatusCode::OK,
                r#"{"error":{"code":502,"message":"Provider returned error"}}"#,
            )
        };
        let accepted = failed_with([("bedrock", envelope())]).await;
        assert!(may_clear(&accepted), "a 200 error envelope: {accepted}");
        let accepted_later =
            failed_with([("bedrock", http_status(401)), ("anthropic", envelope())]).await;
        assert!(may_clear(&accepted_later), "{accepted_later}");

        let views = Arc::new(Views::too_large_for(["anthropic"]));
        let oversized = chain_over(
            vec![
                Box::new(Scripted::failing("bedrock", http_status(401))),
                Box::new(Scripted::ok("anthropic")),
            ],
            views,
            CallSlot::default(),
        )
        .completion(request())
        .await
        .expect_err("the head refused and the tail could not hold the call");
        assert!(may_clear(&oversized), "{oversized}");
        let label = exhaustion(oversized);
        assert!(
            label.contains("401") && !label.contains("terminally"),
            "{label}"
        );

        let fault =
            failed_with([("bedrock", ProviderError::Provider("no such model".into()))]).await;
        assert!(!may_clear(&fault), "a provider-reported fault: {fault}");
    }

    /// What the round says of a refusal is about the view that was refused: the
    /// first model that refused one that put a role after itself, for a chain as
    /// for a lone model. A model that failed some other way, or was refused a
    /// view that did not, is not it.
    #[tokio::test]
    async fn a_refusal_is_kept_with_the_view_that_was_refused() {
        let refusal = |views: Views, statuses: &[(&'static str, u16)]| {
            let slot = CallSlot::default();
            let candidates: Vec<Box<dyn Candidate>> = statuses
                .iter()
                .map(|&(name, status)| {
                    Box::new(Scripted::failing(name, http_status(status))) as Box<dyn Candidate>
                })
                .collect();
            let chain = chain_over(candidates, Arc::new(views), slot.clone());
            async move {
                chain
                    .completion(request())
                    .await
                    .expect_err("every one failed");
                slot.refusal()
                    .map(|Refusal { budget, adjacent }| (budget.model, adjacent.turn))
            }
        };
        let both = Views::adjacent_for(["bedrock", "anthropic"]);
        assert_eq!(
            refusal(both, &[("bedrock", 400), ("anthropic", 401)]).await,
            Some(("bedrock".to_string(), Some(2))),
            "the head's, when the head was refused"
        );
        let both = Views::adjacent_for(["bedrock", "anthropic"]);
        assert_eq!(
            refusal(both, &[("bedrock", 503), ("anthropic", 422)]).await,
            Some(("anthropic".to_string(), Some(2))),
            "a later model's, when the head failed some other way"
        );
        let both = Views::adjacent_for(["bedrock", "anthropic"]);
        assert_eq!(
            refusal(both, &[("bedrock", 400), ("anthropic", 400)]).await,
            Some(("bedrock".to_string(), Some(2))),
            "the first of two"
        );
        assert_eq!(
            refusal(
                Views::adjacent_for(["anthropic"]),
                &[("bedrock", 400), ("anthropic", 401)]
            )
            .await,
            None,
            "a refusal of a view that put no role after itself has nothing to say"
        );
        assert_eq!(
            refusal(Views::adjacent_for(["bedrock"]), &[("bedrock", 503)]).await,
            None,
            "nor does any other failure"
        );
        assert_eq!(
            refusal(Views::adjacent_for(["bedrock"]), &[("bedrock", 400)]).await,
            Some(("bedrock".to_string(), Some(2))),
            "a lone model's refusal, whose error the chain returns as it stands"
        );
    }

    /// What a provider ends an empty reply with when it refuses what the
    /// request carried, as the retry layer words it.
    fn filtered() -> ProviderError {
        ProviderError::Response("finish reason content_filter: empty".into())
    }

    /// A model that refused the call for what it carried is sent it once more,
    /// without the results of the turn it answers, and its answer keeps them
    /// out of every later call.
    #[tokio::test]
    async fn a_call_refused_for_what_it_carried_is_sent_once_more_without_its_results() {
        let slot = CallSlot::default();
        let head = Scripted::new("bedrock", vec![Err(filtered()), Ok("hi")]).filtering(&slot);
        let calls = Arc::clone(&head.calls);
        let views = Arc::new(Views::answering(3));

        let reply = chain_over(
            vec![Box::new(head), Box::new(Scripted::ok("anthropic"))],
            Arc::clone(&views) as Arc<dyn View>,
            slot,
        )
        .completion(request())
        .await
        .expect("answered without them");

        assert_eq!(reply.origin.model, "bedrock", "not moved on");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(views.models(), ["bedrock", "bedrock"]);
        assert_eq!(*views.without.lock().expect("without"), ["bedrock"]);
        assert_eq!(
            *views.withheld.lock().expect("withheld"),
            [Refused {
                turn: 3,
                finish_reason: "content_filter".to_string(),
            }]
        );
    }

    /// Refused again, nothing is kept out: the call moves to the next model,
    /// which is sent the whole view. Neither a failure of another kind nor a
    /// call answering a round's opening, which has no results, is sent again.
    #[tokio::test]
    async fn a_call_refused_again_without_its_results_moves_on_whole() {
        let slot = CallSlot::default();
        let views = Arc::new(Views::answering(3));
        let reply = chain_over(
            vec![
                Box::new(Scripted::failing("bedrock", filtered()).filtering(&slot)),
                Box::new(Scripted::ok("anthropic")),
            ],
            Arc::clone(&views) as Arc<dyn View>,
            slot,
        )
        .completion(request())
        .await
        .expect("the second answers");

        assert_eq!(reply.origin.model, "anthropic");
        assert_eq!(views.models(), ["bedrock", "bedrock", "anthropic"]);
        assert_eq!(*views.without.lock().expect("without"), ["bedrock"]);
        assert!(views.withheld.lock().expect("withheld").is_empty());

        let moved_on = |views: Views, head: Scripted, slot: CallSlot| {
            let views = Arc::new(views);
            let chain = chain_over(
                vec![Box::new(head), Box::new(Scripted::ok("anthropic"))],
                Arc::clone(&views) as Arc<dyn View>,
                slot,
            );
            async move {
                chain
                    .completion(request())
                    .await
                    .expect("the second answers");
                (views.models(), views.without.lock().expect("without").len())
            }
        };
        let moved = || (vec!["bedrock".to_string(), "anthropic".to_string()], 0);
        // The slot is never told the failure was a refusal of what it carried.
        let other = Scripted::failing("bedrock", filtered());
        assert_eq!(
            moved_on(Views::answering(3), other, CallSlot::default()).await,
            moved(),
            "a failure of another kind"
        );
        let slot = CallSlot::default();
        let opening = Scripted::failing("bedrock", filtered()).filtering(&slot);
        assert_eq!(
            moved_on(Views::default(), opening, slot).await,
            moved(),
            "a call answering a round's opening"
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
            matches!(err, ProviderError::ProviderResponse(_)),
            "a chain of one must not wrap its candidate's error: {err:?}"
        );
    }

    /// A request refused because the session's token budget is spent ends the
    /// call there: every candidate would be refused the same, so none after
    /// it is tried, and the refusal comes back as it stands.
    #[tokio::test]
    async fn a_spent_budget_tries_no_other_candidate() {
        let second = Scripted::ok("anthropic");
        let calls = Arc::clone(&second.calls);
        let err = chain(vec![
            Box::new(Scripted::failing("bedrock", spent())),
            Box::new(second),
        ])
        .completion(request())
        .await
        .expect_err("the budget is spent");

        assert!(BudgetSpent::of(&err), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "the second is not tried");
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

    /// Every candidate is sent a view assembled for its own window, the head's
    /// included, after the system prompt rig put first.
    #[tokio::test]
    async fn each_candidate_is_sent_the_view_assembled_for_it() {
        let first = Scripted::failing("bedrock", http_status(503));
        let second = Scripted::ok("anthropic");
        let (first_seen, second_seen) = (Arc::clone(&first.seen), Arc::clone(&second.seen));
        let views = Arc::new(Views::default());

        chain_over(
            vec![Box::new(first), Box::new(second)],
            Arc::clone(&views) as Arc<dyn View>,
            CallSlot::default(),
        )
        .completion(request())
        .await
        .expect("the second candidate answers");

        let history = |seen: &Mutex<Vec<CompletionRequest>>| -> Vec<Message> {
            seen.lock().expect("seen")[0]
                .chat_history
                .iter()
                .map(|message| Message::clone(message))
                .collect()
        };
        assert_eq!(
            history(&first_seen),
            [
                Message::system("preamble"),
                Message::user("the view for bedrock")
            ]
        );
        assert_eq!(
            history(&second_seen),
            [
                Message::system("preamble"),
                Message::user("the view for anthropic")
            ]
        );
        let models: Vec<_> = views.asked().into_iter().map(|(model, _)| model).collect();
        assert_eq!(models, ["bedrock", "anthropic"]);
    }

    /// A candidate whose window the call's latest turn does not fit is passed
    /// over, not called, and its reason is reported with the rest.
    #[tokio::test]
    async fn a_candidate_too_small_for_the_call_is_passed_over() {
        let small = Scripted::ok("small");
        let small_calls = Arc::clone(&small.calls);
        let views = Arc::new(Views::too_large_for(["small"]));

        let reply = chain_over(
            vec![
                Box::new(Scripted::failing("head", http_status(503))),
                Box::new(small),
                Box::new(Scripted::ok("large")),
            ],
            views,
            CallSlot::default(),
        )
        .completion(request())
        .await
        .expect("the third candidate answers");

        assert_eq!(small_calls.load(Ordering::SeqCst), 0, "it was not called");
        assert_eq!(reply.origin.model, "large");

        let err = chain_over(
            vec![
                Box::new(Scripted::failing("head", http_status(503))),
                Box::new(Scripted::ok("small")),
            ],
            Arc::new(Views::too_large_for(["small"])),
            CallSlot::default(),
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

    /// The head is passed over like any other candidate: a call whose latest
    /// turn is too large for its window goes to the next, rather than ending
    /// where the head's window did.
    #[tokio::test]
    async fn a_head_too_small_for_the_call_is_passed_over() {
        let head = Scripted::ok("small");
        let head_calls = Arc::clone(&head.calls);
        let reply = chain_over(
            vec![Box::new(head), Box::new(Scripted::ok("large"))],
            Arc::new(Views::too_large_for(["small"])),
            CallSlot::default(),
        )
        .completion(request())
        .await
        .expect("the second candidate's window holds it");

        assert_eq!(head_calls.load(Ordering::SeqCst), 0, "it was not called");
        assert_eq!(reply.origin.model, "large");
    }

    /// A call is not made, and says why, when every candidate's window is too
    /// small for it -- each one's reason, or the lone model's own. One that
    /// was sent the call and failed is a failure instead, whatever the others'
    /// windows held.
    #[tokio::test]
    async fn a_call_no_window_holds_is_unfit_and_any_other_failure_is_not() {
        let oversized = |names: &'static [&'static str], views: Views| {
            let candidates: Vec<Box<dyn Candidate>> = names
                .iter()
                .map(|name| Box::new(Scripted::ok(name)) as Box<dyn Candidate>)
                .collect();
            chain_over(candidates, Arc::new(views), CallSlot::default())
        };

        let err = oversized(
            &["small", "smaller"],
            Views::too_large_for(["small", "smaller"]),
        )
        .completion(request())
        .await
        .expect_err("no window holds the turn");
        let reason = unfit(&err).expect("every candidate was passed over");
        assert!(
            reason.starts_with("no model candidate has room for the call; tried:\n  small   -- ")
                && reason.contains("\n  smaller -- turn 3 of round 1"),
            "{reason}"
        );
        assert!(
            may_clear(&err),
            "the turn stays in the conversation, and the next view leaves it out"
        );

        let lone = oversized(&["small"], Views::too_large_for(["small"]))
            .completion(request())
            .await
            .expect_err("the one window is too small");
        let reason = unfit(&lone).expect("the lone candidate was passed over");
        assert!(
            reason.starts_with("turn 3 of round 1 -- the model's last call and what it returned"),
            "a lone model's reason is its own: {reason}"
        );

        let failed = chain_over(
            vec![
                Box::new(Scripted::ok("small")),
                Box::new(Scripted::failing("down", http_status(503))),
            ],
            Arc::new(Views::too_large_for(["small"])),
            CallSlot::default(),
        )
        .completion(request())
        .await
        .expect_err("the only candidate that could hold it failed");
        assert_eq!(unfit(&failed), None, "{failed}");
        assert_eq!(unfit(&http_status(503)), None);
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
            CallSlot::new(
                Ledger::new(events.clone(), None, None),
                events.clone(),
                None,
            ),
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
        assert_eq!(moves[0]["call_id"], 1, "the call that moved");
        assert_eq!(
            moves[0]["attempt_id"],
            serde_json::Value::Null,
            "a candidate with no request layer sent nothing this chain recorded"
        );
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
        assert!(!mixed.capabilities.composes_native_output_with_tools);

        let homogeneous = chain(vec![
            Box::new(Scripted::ok("a").composing(true)),
            Box::new(Scripted::ok("b").composing(true)),
        ]);
        assert!(
            homogeneous.capabilities.composes_native_output_with_tools,
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
            (moved.origin.model, head.origin.model),
            ("anthropic".to_string(), "bedrock".to_string())
        );
    }

    /// A chain of one candidate whose answers report `reported` input tokens,
    /// called twice: the chain, and the scale each call was held to.
    async fn answered_twice(reported: u64) -> (FailoverModel, Vec<Scale>) {
        let views = Arc::new(Views::default());
        let model = chain_over(
            vec![Box::new(Scripted::ok("opus").reporting(reported))],
            Arc::clone(&views) as Arc<dyn View>,
            CallSlot::default(),
        );
        model.completion(request()).await.expect("answers");
        model.completion(request()).await.expect("answers");
        let scales = views.asked().into_iter().map(|(_, scale)| scale).collect();
        (model, scales)
    }

    /// An answer whose request its provider reported over the estimate scales
    /// the next call to that candidate by the ratio, which a round opens
    /// against as well.
    #[tokio::test]
    async fn an_answer_over_its_estimate_scales_the_next_call() {
        let (model, scales) = answered_twice(ESTIMATE * 6 / 5).await;

        let reported = Scale::observed(6, 5);
        assert_eq!(
            scales,
            [Scale::RULE, reported],
            "the first call is held to the rule, and the second to what was reported"
        );
        assert_eq!(model.head_budget().scale, reported);
    }

    /// A provider that reported less than the estimate, or nothing, moves the
    /// scale below nothing: it is never under the rule.
    #[tokio::test]
    async fn an_answer_under_its_estimate_or_with_no_usage_leaves_the_rule() {
        for reported in [ESTIMATE / 2, 0] {
            let (_, scales) = answered_twice(reported).await;
            assert_eq!(
                scales,
                [Scale::RULE, Scale::RULE],
                "reported {reported} of an estimate of {ESTIMATE}"
            );
        }
    }

    /// A view that had to lower the reply's ceiling to carry the call's prompt
    /// -- the scale would have refused it -- sets the ceiling the call is sent,
    /// not the candidate's.
    #[tokio::test]
    async fn a_ceiling_the_view_lowered_is_the_one_sent() {
        let head = Scripted::ok("opus").with_max_tokens(64_000);
        let seen = Arc::clone(&head.seen);
        let views = Views {
            lowered_to: Some(1_500),
            ..Views::default()
        };
        let model = chain_over(vec![Box::new(head)], Arc::new(views), CallSlot::default());

        model.completion(request()).await.expect("answers");

        let ceilings: Vec<_> = seen
            .lock()
            .expect("seen")
            .iter()
            .map(|request| request.max_tokens)
            .collect();
        assert_eq!(ceilings, [Some(1_500)]);
    }

    /// What one candidate reported says nothing of another: a call that moves
    /// from a head with a ratio to its fallback starts the fallback from the
    /// rule, and the fallback's answer scales only the fallback.
    #[tokio::test]
    async fn a_failover_starts_from_the_rule() {
        // Answers once, reporting 1.2x, then fails.
        let head = Scripted::new("bedrock", vec![Ok("hi"), Err(http_status(503))])
            .reporting(ESTIMATE * 6 / 5);
        let fallback = Scripted::ok("anthropic").reporting(ESTIMATE * 3 / 2);
        let views = Arc::new(Views::default());
        let model = chain_over(
            vec![Box::new(head), Box::new(fallback)],
            Arc::clone(&views) as Arc<dyn View>,
            CallSlot::default(),
        );

        model.completion(request()).await.expect("the head answers");
        model
            .completion(request())
            .await
            .expect("the fallback answers");
        // The fallback's own ratio is its own: the head is still at 1.2.
        model
            .completion(request())
            .await
            .expect("the head fails again");

        let (head, fallback) = (Scale::observed(6, 5), Scale::observed(3, 2));
        let asked = |model: &str, scale| (model.to_string(), scale);
        assert_eq!(
            views.asked(),
            [
                asked("bedrock", Scale::RULE),
                asked("bedrock", head),
                asked("anthropic", Scale::RULE),
                asked("bedrock", head),
                asked("anthropic", fallback),
            ]
        );
        assert_eq!(model.head_budget().scale, head);
    }

    /// Streaming reaches no candidate at all: the agent loop never streams, and
    /// a chain refuses rather than fanning a stream out.
    #[tokio::test]
    async fn streaming_is_refused_rather_than_fanned_out() {
        let candidate = Scripted::ok("only");
        let calls = Arc::clone(&candidate.calls);
        let streamed = EffectKind::Completion {
            request: Arc::new(request()),
            stream: true,
        };
        let reply = chain(vec![Box::new(candidate)])
            .serve(
                streamed,
                Dispatch::new(rig::effect::EffectId::from_raw(1), true),
            )
            .await;
        assert!(matches!(reply, Reply::Outcome(Err(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// What leaves the chain is a report the round can read: a call not made
    /// for want of room, the spent budget, and an exhaustion each carry their
    /// code, and whether the failure may clear rides as `retryable`.
    #[test]
    fn a_failure_leaves_the_chain_as_a_report_naming_its_class() {
        let spent = report(spent());
        assert!(budget_spent_report(&spent));
        assert!(!spent.retryable);

        let too_large = report(ProviderError::request(TooLarge {
            turn: Some(3),
            round: 1,
            estimate: 9_000,
            budget: budget("only", None),
        }));
        assert!(unfit_report(&too_large).is_some(), "{too_large:?}");
        assert!(too_large.retryable);

        let exhausted = report(Exhausted::into_error(vec![
            Abandoned {
                model_name: "bedrock".into(),
                error: http_status(401),
            },
            Abandoned {
                model_name: "anthropic".into(),
                error: http_status(503),
            },
        ]));
        assert_eq!(exhausted.code.as_deref(), Some(CODE_EXHAUSTED));
        assert!(exhausted.retryable, "the 503 may lift");
        assert!(
            exhausted.message.contains("bedrock") && exhausted.message.contains("anthropic"),
            "{}",
            exhausted.message
        );

        let terminal = report(http_status(401));
        assert!(!terminal.retryable);
        assert!(unfit_report(&terminal).is_none());
        assert!(!budget_spent_report(&terminal));
    }
}
