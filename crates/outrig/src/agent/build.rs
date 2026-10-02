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

use rig::agent::{Agent, AgentBuilder};
use rig::client::CompletionClient;
use rig::completion::CompletionModel;
use rig::providers::{anthropic, openai};
use rig::tool::ToolDyn;

use super::AgentError;
use super::budget::Budget;
use super::failover::{Candidate, FailoverModel, ModelCandidate};
use super::history::History;
use super::resolve::{LlmResolveError, ResolvedAgent, ResolvedCandidate, ResolvedProvider};
use super::retry::{self, Retries, RetryPolicy, RetryingHttpClient, RetryingModel};
use crate::events::Events;

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

/// A Rig agent over a failover chain of the resolved candidates.
pub(crate) type RigAgent = Agent<FailoverModel>;

/// Build the agent for `resolved`, with `preamble` as its system prompt and
/// `tools` as its whole tool list, recording to `history`'s events and taking
/// a moved call's view from `history`. Every call carries `overhead` besides
/// the conversation. Does no I/O.
///
/// Each candidate's [`Budget`] is settled and checked here, since its reserve
/// is the ceiling building settles: one whose window leaves a round too little
/// room fails the build, whichever candidate it is. The chain holds them
/// ([`FailoverModel::budgets`]).
pub(crate) fn build_agent(
    resolved: &ResolvedAgent,
    preamble: &str,
    tools: Vec<Box<dyn ToolDyn>>,
    history: &History,
    overhead: u64,
) -> Result<RigAgent, AgentError> {
    // One policy for the whole chain, so its `chain_deadline` is the same
    // handle in every candidate's HTTP client and model wrapper. Arming it once
    // per `completion()` call is what bounds the chain's worst case at one
    // budget rather than one per candidate.
    //
    // The budget itself comes from the head's provider. A chain spanning
    // providers that disagree on `retry-budget-secs` has no single right
    // answer, and the head of a preference order is the defensible one -- it
    // is the endpoint the user said to use.
    let policy = retry_policy(resolved.head().provider.retry_budget_secs());
    let events = history.events();
    let candidates = resolved
        .candidates
        .iter()
        .map(|candidate| build_candidate(candidate, &policy, events, overhead))
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

    let chain = FailoverModel::new(
        candidates,
        policy,
        events.clone(),
        Arc::new(history.clone()),
    );
    // No `max_tokens` here: the chain sets each attempt's to its candidate's.
    let mut builder = AgentBuilder::new(chain).preamble(preamble);
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(f64::from(temperature));
    }
    Ok(builder.tools(tools).build())
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
    events: &Events,
    overhead: u64,
) -> Result<Box<dyn Candidate>, AgentError> {
    let retries = Retries::new(events.clone(), &candidate.model_name);
    let http = remote_http_client(candidate.provider.request_timeout_secs(), policy, &retries)?;
    match &candidate.provider {
        ResolvedProvider::OpenAi {
            base_url, api_key, ..
        } => {
            let client = openai::CompletionsClient::builder()
                .api_key(api_key.to_string())
                .base_url(base_url)
                .http_client(http)
                .build()
                .map_err(client_build)?;
            let model = client.completion_model(&candidate.model_identifier);
            link(
                candidate,
                policy,
                retries,
                overhead,
                model,
                candidate.max_tokens,
            )
        }
        ResolvedProvider::Anthropic {
            base_url, api_key, ..
        } => {
            // Rig's client owns the protocol: `x-api-key`, the
            // `anthropic-version` header, `POST {base-url}/v1/messages`, and the
            // native content blocks. It also normalizes a trailing `/v1` or
            // `/messages` off the configured base URL.
            let client = anthropic::Client::builder()
                .api_key(api_key.to_string())
                .base_url(base_url)
                .http_client(http)
                .build()
                .map_err(client_build)?;
            let (model, max_tokens) = anthropic_model(&client, candidate);
            link(
                candidate,
                policy,
                retries,
                overhead,
                model,
                Some(max_tokens),
            )
        }
    }
}

/// `model`, retried under `policy` and recorded by `retries`, as `candidate`'s
/// link of the chain: its calls carry `max_tokens`, and `overhead` besides the
/// conversation. What every provider's link shares.
fn link<M>(
    candidate: &ResolvedCandidate,
    policy: &RetryPolicy,
    retries: Retries,
    overhead: u64,
    model: M,
    max_tokens: Option<u32>,
) -> Result<Box<dyn Candidate>, AgentError>
where
    M: CompletionModel + Send + Sync + 'static,
{
    let budget = Budget::new(candidate, max_tokens, overhead)?;
    tracing::debug!(
        model = %candidate.model_name,
        identifier = %candidate.model_identifier,
        provider = %candidate.provider_name,
        max_tokens = budget.max_tokens,
        window = budget.window,
        assumed = budget.window_assumed,
        reserve = budget.reserve,
        overhead = budget.overhead,
        "built a candidate; each call to it is held to this budget"
    );
    Ok(Box::new(ModelCandidate::new(
        RetryingModel::new(model, policy.clone(), retries),
        &candidate.model_identifier,
        budget,
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
    // Two ceilings, on two different things. `timeout` bounds the whole
    // request, and is high because a non-streaming completion answers only
    // when the model has finished. `connect_timeout` bounds getting connected
    // at all, which no completion is waiting on -- without it, a host that
    // silently drops packets would hold an attempt open for the full request
    // timeout, and the retry loop's much shorter budget for an endpoint that
    // never answered could not stop it (see [`retry::CONNECT_TIMEOUT`]).
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

    let inner = builder.build().map_err(client_build)?;
    Ok(RetryingHttpClient::new(
        inner,
        policy.clone(),
        retries.clone(),
    ))
}

/// One candidate's Anthropic model, and the output-token ceiling that reaches
/// the wire with it.
///
/// Precedence, highest first: the agent's or model's `max-tokens`, then rig's
/// published ceiling for an identifier it recognizes, then
/// [`ANTHROPIC_FALLBACK_MAX_TOKENS`]. There is always one, because Anthropic
/// rejects a request that carries none. Under a chain it runs per candidate,
/// which is what makes the ceiling travel with the identifier instead of
/// staying the head's.
pub(crate) fn anthropic_model(
    client: &anthropic::Client<RetryingHttpClient>,
    candidate: &ResolvedCandidate,
) -> (
    anthropic::completion::CompletionModel<RetryingHttpClient>,
    u32,
) {
    // `completion_model`, never `CompletionModel::with_model`: the two
    // disagree about a model identifier rig does not recognize. This one
    // leaves the default unset, which is the signal the fallback below keys
    // off. `with_model` would have already capped every reply at 2048 tokens,
    // silently.
    let mut model = client.completion_model(&candidate.model_identifier);
    let max_tokens = match (candidate.max_tokens, model.default_max_tokens) {
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
            model.default_max_tokens = Some(u64::from(ANTHROPIC_FALLBACK_MAX_TOKENS));
            u64::from(ANTHROPIC_FALLBACK_MAX_TOKENS)
        }
    };
    (model, u32::try_from(max_tokens).unwrap_or(u32::MAX))
}
