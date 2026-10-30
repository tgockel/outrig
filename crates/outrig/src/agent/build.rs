//! Build a Rig agent for a resolved agent: a failover chain over its
//! candidates, each with its client, its retry stack, and what a call to it may
//! carry -- including the output-token ceiling it will actually be held to.
//!
//! Copied from `outrig-cli`'s `llm.rs`. The CLI builds a lone model without a
//! chain, so its sessions stay byte-for-byte what they were before failover;
//! here every agent's model is a chain, of one when no alias names more.

use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use rig::DynModel;
use rig::agent::{Agent, AgentBuilder};
use rig::driver::Model;
use rig::effect::model_key;
use rig::handlers::Handlers;
use rig::http_client::DynHttpClient;
use rig::operation::Completion;
use rig::providers::anthropic::wire::{
    ANTHROPIC, AnthropicConfig, Dialect, Messages, Quirks, ToolInputStreaming,
};
use rig::providers::openai::OpenAIConfig;
use rig::tool::Tool;

use super::AgentError;
use super::budget::Budget;
use super::failover::{Binding, CHAIN_LABEL, Candidate, FailoverModel, ModelCandidate};
use super::history::History;
use super::ledger::CallSlot;
use super::resolve::{LlmResolveError, ResolvedAgent, ResolvedCandidate, ResolvedProvider};
use super::retry::{self, Retries, RetryPolicy, RetryingHttpClient, RetryingModel};

/// Default per-request HTTP timeout for remote providers when
/// `request-timeout-secs` is unset. Generous enough not to truncate long
/// reasoning completions, and above typical proxy timeouts so a client-side
/// timeout never races a still-in-flight server request.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

/// Output-token ceiling for a Claude identifier this build of rig has no
/// published ceiling for -- typically a model newer than the pinned rig, or a
/// proxy's own naming. Anthropic rejects a request that carries no ceiling at
/// all, so *something* has to fill the gap.
///
/// High enough that a real reply is not clipped, and below every
/// current-generation ceiling. An identifier whose true limit is *lower* draws
/// a 400 from Anthropic naming that limit -- loud, and fixable with one config
/// line, which is the trade this number is chosen for.
pub(crate) const ANTHROPIC_FALLBACK_MAX_TOKENS: u32 = 32_768;

/// Anthropic's own dialect under another name, for a model built not to ask
/// the API to drop stale thinking: rig asks only on the dialect it knows as
/// Anthropic's, so a request through this one carries neither the field nor
/// its beta. Same endpoint, same key, same quirks.
const ANTHROPIC_UNBOUND: Dialect = Dialect {
    name: "anthropic-unbound",
    quirks: Quirks::anthropic(),
    ..ANTHROPIC
};

/// A Rig agent over a failover chain of the resolved candidates: rig's runner,
/// and the chain it dispatches every model call to, which is what the round
/// reads of the agent's model.
pub(crate) struct RigAgent {
    pub(crate) agent: Agent,
    pub(crate) model: FailoverModel,
}

impl RigAgent {
    /// Hold calls to the chain's head to a `window` of tokens in place of the
    /// one it was built with, for a test that could not know to configure it.
    /// Only while no call is in flight: the agent's handler table holds the
    /// chain too, so it is taken out while the head changes and put back.
    #[cfg(test)]
    pub(crate) fn narrow_head(&mut self, window: u32) {
        let key = model_key(CHAIN_LABEL);
        self.agent.handlers().deregister(&key);
        self.model.narrow_head(window);
        self.agent
            .handlers()
            .register(key, self.model.clone())
            .expect("the chain registers again");
    }
}

/// Build the agent for `resolved`, with `preamble` as its system prompt and
/// `tool` as its whole tool list, naming its calls and attempts in `slot` and
/// taking a moved call's view from `history`. Every call carries `overhead`
/// besides the conversation. Does no I/O.
///
/// Each candidate's [`Budget`] is settled and checked here, since its reserve
/// is the ceiling building settles: one whose window leaves a round too little
/// room fails the build, whichever candidate it is. The chain holds them
/// ([`FailoverModel::budgets`]).
pub(crate) fn build_agent<T: Tool + 'static>(
    resolved: &ResolvedAgent,
    preamble: &str,
    tool: T,
    history: &History,
    slot: CallSlot,
    overhead: u64,
) -> Result<RigAgent, AgentError> {
    // One policy for the whole chain, so its `chain_deadline` is the same
    // handle in every candidate's HTTP client and model wrapper. Arming it once
    // per completion is what bounds the chain's worst case at one budget rather
    // than one per candidate.
    //
    // The budget itself comes from the head's provider. A chain spanning
    // providers that disagree on `retry-budget-secs` has no single right
    // answer, and the head of a preference order is the defensible one -- it
    // is the endpoint the user said to use.
    let policy = retry_policy(resolved.head().provider.retry_budget_secs());
    let candidates = resolved
        .candidates
        .iter()
        .map(|candidate| build_candidate(candidate, &policy, &slot, overhead))
        .collect::<Result<Vec<_>, _>>()?;
    if let [head, rest @ ..] = &resolved.candidates[..]
        && !rest.is_empty()
    {
        // A move means one round can be half one model's work, so the models
        // it may move to are named before the first one, as `run`'s banner
        // names them.
        let rest: Vec<&str> = rest.iter().map(|c| c.model_name.as_str()).collect();
        tracing::info!(
            "calls go to {}, and one that fails moves to {}, in that order",
            head.model_name,
            rest.join(", ")
        );
    }

    let chain = FailoverModel::new(candidates, policy, slot, Arc::new(history.clone()));
    // The chain is the agent's model: rig dispatches every model call to the
    // handler registered under the label the agent is built over.
    let handlers = Handlers::default();
    handlers
        .register(model_key(CHAIN_LABEL), chain.clone())
        .map_err(|report| client_build(report.message))?;
    // No `max_tokens` here: the chain sets each attempt's to its candidate's.
    // A finish reason rig does not know is a normal stop, as it was before
    // rig told them apart: a proxy's own word for one must not fail a round.
    let mut builder = AgentBuilder::over_handlers(handlers, CHAIN_LABEL, model_key(CHAIN_LABEL))
        .preamble(preamble)
        .accept_unknown_finish_reasons(true);
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(f64::from(temperature));
    }
    Ok(RigAgent {
        agent: builder.tool(tool).build(),
        model: chain,
    })
}

/// Build one link of the chain: its client, its retry stack, its model, and
/// its budget, behind the object-safe shim a chain holds.
///
/// Every candidate takes the *chain's* policy rather than its own provider's,
/// so they share one deadline. The rest -- the client, the ceiling precedence,
/// the Anthropic cap, the window -- is per candidate.
fn build_candidate(
    candidate: &ResolvedCandidate,
    policy: &RetryPolicy,
    slot: &CallSlot,
    overhead: u64,
) -> Result<Box<dyn Candidate>, AgentError> {
    let retries = Retries::new(slot.clone(), &candidate.model_name);
    let http = DynHttpClient::new(remote_http_client(
        candidate.provider.request_timeout_secs(),
        policy,
        &retries,
    )?);
    match &candidate.provider {
        ResolvedProvider::OpenAi {
            base_url, api_key, ..
        } => {
            // Chat Completions, never Responses: an OpenAI-compatible proxy
            // serves `/chat/completions`, and the configured `base-url` is
            // sent as written.
            let model = OpenAIConfig::new(api_key.expose())
                .with_base_url(base_url)
                .connect(http)
                .chat(&candidate.model_identifier);
            link(
                candidate,
                policy,
                retries,
                overhead,
                model,
                candidate.max_tokens,
                None,
            )
        }
        ResolvedProvider::Anthropic {
            base_url, api_key, ..
        } => {
            // Rig's client owns the protocol: `x-api-key`, the
            // `anthropic-version` header, `POST {base-url}/v1/messages`, and the
            // native content blocks. It also normalizes a trailing `/v1` or
            // `/messages` off the configured base URL.
            //
            // Calls ask for `drop_block`, as rig does for a model it knows to
            // bind its thinking, until the endpoint refuses that and they go
            // through the same model built not to ask.
            let (bound, max_tokens) = anthropic_model(
                AnthropicConfig::new(api_key.expose()).with_base_url(base_url),
                http.clone(),
                candidate,
            );
            let (unbound, _) = anthropic_model(
                AnthropicConfig::with_key(&ANTHROPIC_UNBOUND, api_key.expose())
                    .with_base_url(base_url),
                http,
                candidate,
            );
            link(
                candidate,
                policy,
                retries,
                overhead,
                bound,
                Some(max_tokens),
                Some(unbound),
            )
        }
    }
}

/// `model`, retried under `policy` and recorded by `retries`, as `candidate`'s
/// link of the chain: its calls carry `max_tokens`, and `overhead` besides the
/// conversation. With `unbound`, they go through it instead once the endpoint
/// has refused a request that asked for `drop_block`. What every provider's
/// link shares.
fn link<M>(
    candidate: &ResolvedCandidate,
    policy: &RetryPolicy,
    retries: Retries,
    overhead: u64,
    model: M,
    max_tokens: Option<u32>,
    unbound: Option<M>,
) -> Result<Box<dyn Candidate>, AgentError>
where
    M: Into<DynModel<Completion>>,
{
    let budget = Budget::new(candidate, max_tokens, overhead)?;
    tracing::debug!(
        model = %candidate.model_name,
        identifier = %candidate.model_identifier,
        provider = %candidate.provider_name,
        max_tokens = budget.max_tokens,
        window = budget.window,
        window_source = ?budget.window_source,
        reserve = budget.reserve,
        overhead = budget.overhead,
        "built a candidate; each call to it is held to this budget"
    );
    let binding = unbound
        .map(|model| Binding::new(RetryingModel::new(model, policy.clone(), retries.clone())));
    Ok(Box::new(ModelCandidate::new(
        RetryingModel::new(model, policy.clone(), retries),
        &candidate.model_identifier,
        budget,
        binding,
    )))
}

fn client_build(e: impl Display) -> AgentError {
    LlmResolveError::RigClientBuild(e.to_string()).into()
}

/// The retry policy every candidate gets, from the head's `retry-budget-secs`
/// or [`DEFAULT_RETRY_BUDGET_SECS`]. Both retry layers take the same one, so
/// `retry-budget-secs = 0` switches off both.
///
/// `retry-budget-secs` is not the whole policy: the default carries a second,
/// much shorter bound for a call that never reaches the endpoint, which config
/// cannot reach. It never widens this one -- the loop applies whichever is
/// smaller -- so `0` still means no retries anywhere. See
/// [`RetryPolicy::connect_budget`].
///
/// [`DEFAULT_RETRY_BUDGET_SECS`]: crate::config::DEFAULT_RETRY_BUDGET_SECS
fn retry_policy(retry_budget_secs: Option<u64>) -> RetryPolicy {
    RetryPolicy {
        budget: Duration::from_secs(
            retry_budget_secs.unwrap_or(crate::config::DEFAULT_RETRY_BUDGET_SECS),
        ),
        ..RetryPolicy::default()
    }
}

/// The client builder every request to a provider starts from: `timeout` on the
/// whole request, and the connect bound.
///
/// Two ceilings, on two different things. `timeout` bounds the whole request,
/// and is high for a model call because a non-streaming completion answers only
/// when the model has finished. `connect_timeout` bounds getting connected at
/// all, which no completion is waiting on -- without it, a host that silently
/// drops packets would hold an attempt open for the full request timeout, and
/// the retry loop's much shorter budget for an endpoint that never answered
/// could not stop it (see [`retry::CONNECT_TIMEOUT`]).
pub(super) fn http_client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(retry::CONNECT_TIMEOUT);

    // Unit tests point providers at loopback fixtures. reqwest picks up
    // `HTTP_PROXY` / `ALL_PROXY` automatically and has no loopback exemption of
    // its own, so on a machine behind a proxy every fixture request would be
    // sent to that proxy instead of the fixture. Real runs keep automatic proxy
    // detection, which is how a user reaches a hosted provider from behind one.
    #[cfg(test)]
    let builder = builder.no_proxy();

    builder
}

/// The HTTP client every remote provider gets: one per-request timeout, from
/// the provider's `request-timeout-secs` or [`DEFAULT_REQUEST_TIMEOUT_SECS`],
/// and the connect bound, wrapped in the transient-retry loop `policy` bounds
/// and `retries` records.
fn remote_http_client(
    request_timeout_secs: Option<u64>,
    policy: &RetryPolicy,
    retries: &Retries,
) -> Result<RetryingHttpClient, AgentError> {
    let timeout = Duration::from_secs(request_timeout_secs.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));
    let inner = http_client_builder(timeout).build().map_err(client_build)?;
    Ok(RetryingHttpClient::new(
        inner,
        policy.clone(),
        retries.clone(),
    ))
}

/// One candidate's Anthropic model at `config` over `http`, and the
/// output-token ceiling that reaches the wire with it.
///
/// Precedence, highest first: the agent's or model's `max-tokens`, then rig's
/// published ceiling for an identifier it recognizes, then
/// [`ANTHROPIC_FALLBACK_MAX_TOKENS`]. There is always one, because Anthropic
/// rejects a request that carries none. Under a chain it runs per candidate,
/// which is what makes the ceiling travel with the identifier instead of
/// staying the head's.
///
/// The model sends its tools without `eager_input_streaming`: nothing here
/// streams, and a gateway that does not know the field refuses the request.
pub(crate) fn anthropic_model(
    config: AnthropicConfig,
    http: DynHttpClient,
    candidate: &ResolvedCandidate,
) -> (Model<Messages>, u32) {
    let mut model = config.connect(http).completion(&candidate.model_identifier);
    // rig's own ceiling for the identifier, from its catalog, or none for one
    // it does not recognize -- which is the signal the fallback below keys off.
    let published = model.wire.default_max_tokens;
    let max_tokens = match (candidate.max_tokens, published) {
        // The published ceiling is a cap as well as a default. A configured
        // ceiling above it is a request the API refuses outright, so lowering
        // it is the only outcome that runs at all.
        (Some(want), Some(ceiling)) => u64::from(want).min(ceiling),
        (Some(want), None) => u64::from(want),
        (None, Some(ceiling)) => ceiling,
        (None, None) => {
            // A reply cut off at a ceiling nobody asked for looks like a bad
            // model rather than a config gap, so say so before the first round.
            tracing::warn!(
                "{} has no published output-token ceiling in this build, so rounds are capped \
                 at {ANTHROPIC_FALLBACK_MAX_TOKENS}. Set [models.{}].max-tokens (or the \
                 agent's) to choose your own.",
                candidate.model_identifier,
                candidate.model_name,
            );
            u64::from(ANTHROPIC_FALLBACK_MAX_TOKENS)
        }
    };
    model.wire = model
        .wire
        .with_default_max_tokens(max_tokens)
        .with_tool_input_streaming(ToolInputStreaming::Off);
    (model, u32::try_from(max_tokens).unwrap_or(u32::MAX))
}
