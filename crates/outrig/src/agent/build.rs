//! Build a Rig agent for a resolved agent: its client, its model, and the
//! output-token ceiling the model will actually be held to.
//!
//! Copied from `outrig-cli`'s `llm.rs` without its retry layer, which arrives
//! later: each provider gets a plain `reqwest::Client` rather than the retrying
//! one.

use std::fmt::Display;
use std::time::Duration;

use rig::agent::{Agent, AgentBuilder};
use rig::client::CompletionClient;
use rig::completion::CompletionModel;
use rig::providers::{anthropic, openai};
use rig::tool::ToolDyn;

use super::AgentError;
use super::resolve::{LlmResolveError, ResolvedAgent, ResolvedCandidate, ResolvedProvider};

/// Default per-request HTTP timeout for remote providers when
/// `request-timeout-secs` is unset. Generous enough not to truncate long
/// reasoning completions, and above typical proxy timeouts so a client-side
/// timeout never races a still-in-flight server request.
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 600;

/// How long a request may take to connect at all. The request timeout above
/// bounds a whole non-streaming completion, which answers only when the model
/// has finished; this bounds reaching the endpoint, which nothing is waiting
/// on, so a host that silently drops packets fails in seconds rather than in
/// ten minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// A Rig agent over whichever provider the candidate named. The two
/// `CompletionModel` impls are concretely different types, so the agent is an
/// enum rather than one type.
pub(crate) enum RigAgent {
    OpenAi(Agent<openai::CompletionModel<reqwest::Client>>),
    Anthropic(Agent<anthropic::completion::CompletionModel<reqwest::Client>>),
}

/// What building produced: the agent, and the output-token ceiling it will be
/// held to -- which is not always the one configured, since the Anthropic arm
/// fills it in or lowers it.
pub(crate) struct Built {
    pub(crate) agent: RigAgent,
    pub(crate) max_tokens: Option<u32>,
}

/// Build the agent for `resolved`, with `tools` as its whole tool list. Does
/// no I/O.
pub(crate) fn build_agent(
    resolved: &ResolvedAgent,
    tools: Vec<Box<dyn ToolDyn>>,
) -> Result<Built, AgentError> {
    let candidate = &resolved.candidate;
    let built = match &candidate.provider {
        ResolvedProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
        } => {
            let client = openai::CompletionsClient::builder()
                .api_key(api_key.to_string())
                .base_url(base_url)
                .http_client(remote_http_client(*request_timeout_secs)?)
                .build()
                .map_err(client_build)?;
            let model = client.completion_model(&candidate.model_identifier);
            Built {
                agent: RigAgent::OpenAi(finish_agent(model, resolved, candidate.max_tokens, tools)),
                max_tokens: candidate.max_tokens,
            }
        }
        ResolvedProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
        } => {
            // Rig's client owns the protocol: `x-api-key`, the
            // `anthropic-version` header, `POST {base-url}/v1/messages`, and the
            // native content blocks. It also normalizes a trailing `/v1` or
            // `/messages` off the configured base URL.
            let client = anthropic::Client::builder()
                .api_key(api_key.to_string())
                .base_url(base_url)
                .http_client(remote_http_client(*request_timeout_secs)?)
                .build()
                .map_err(client_build)?;
            let (model, max_tokens) = anthropic_model(&client, candidate);
            Built {
                agent: RigAgent::Anthropic(finish_agent(model, resolved, Some(max_tokens), tools)),
                max_tokens: Some(max_tokens),
            }
        }
    };
    tracing::debug!(
        model = %candidate.model_name,
        identifier = %candidate.model_identifier,
        provider = %candidate.provider_name,
        max_tokens = built.max_tokens,
        "built the agent"
    );
    Ok(built)
}

fn client_build(e: impl Display) -> AgentError {
    LlmResolveError::RigClientBuild(e.to_string()).into()
}

/// The HTTP client every remote provider gets: one per-request timeout, from
/// the provider's `request-timeout-secs` or [`DEFAULT_REQUEST_TIMEOUT_SECS`],
/// and the connect bound.
fn remote_http_client(request_timeout_secs: Option<u64>) -> Result<reqwest::Client, AgentError> {
    let timeout = Duration::from_secs(request_timeout_secs.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));
    let builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT);

    // Unit tests point providers at loopback fixtures. reqwest picks up
    // `HTTP_PROXY` / `ALL_PROXY` automatically and has no loopback exemption of
    // its own, so on a machine behind a proxy every fixture request would be
    // sent to that proxy instead of the fixture. Real runs keep automatic proxy
    // detection, which is how a user reaches a hosted provider from behind one.
    #[cfg(test)]
    let builder = builder.no_proxy();

    builder.build().map_err(client_build)
}

/// One candidate's Anthropic model, and the output-token ceiling that reaches
/// the wire with it.
///
/// Precedence, highest first: the agent's or model's `max-tokens`, then rig's
/// published ceiling for an identifier it recognizes, then
/// [`ANTHROPIC_FALLBACK_MAX_TOKENS`]. There is always one, because Anthropic
/// rejects a request that carries none.
pub(crate) fn anthropic_model(
    client: &anthropic::Client<reqwest::Client>,
    candidate: &ResolvedCandidate,
) -> (anthropic::completion::CompletionModel<reqwest::Client>, u32) {
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

/// `max_tokens` is passed rather than read off `resolved` because the ceiling
/// that travels is not always the one that was configured.
fn finish_agent<M: CompletionModel + 'static>(
    model: M,
    resolved: &ResolvedAgent,
    max_tokens: Option<u32>,
    tools: Vec<Box<dyn ToolDyn>>,
) -> Agent<M> {
    let mut builder = AgentBuilder::new(model);
    // Skipped rather than passed as "": a builder that never saw a preamble
    // sends no system prompt, which is what an unset `preamble` means.
    if let Some(preamble) = &resolved.preamble {
        builder = builder.preamble(preamble);
    }
    if let Some(temperature) = resolved.temperature {
        builder = builder.temperature(f64::from(temperature));
    }
    if let Some(max_tokens) = max_tokens {
        builder = builder.max_tokens(u64::from(max_tokens));
    }
    builder.tools(tools).build()
}
