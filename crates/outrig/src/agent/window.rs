//! What a model's provider says its window is, asked once for a row that sets
//! none.
//!
//! A provider publishes the number, so a row that omits `context-window` is
//! better held to the provider's report than to [`ASSUMED_CONTEXT_WINDOW`]
//! (`budget`). Resolution does no I/O, so this runs after it, as the primary
//! agent is built and as a child is made, and leaves what it learned on the
//! candidate ([`ResolvedCandidate::reported_window`]). It is a request that may
//! fail, and the session does not depend on it: a failure is kept, with its
//! reason, for the budget to fall back on and say why.
//!
//! The answer sets the window only. It is never inferred from the model's
//! identifier, and nothing here reads the reply's ceiling the provider also
//! reports.
//!
//! [`ASSUMED_CONTEXT_WINDOW`]: super::budget::ASSUMED_CONTEXT_WINDOW

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::join_all;
use rig::providers::anthropic::completion::ANTHROPIC_VERSION_LATEST;
use rig::providers::anthropic::wire::normalize_base_url as normalize_anthropic_base_url;
use serde_json::Value;
use tokio::sync::OnceCell;

use super::build::http_client_builder;
use super::resolve::{ResolvedCandidate, ResolvedProvider};
use super::retry::innermost_cause;

/// How long a session waits for one provider's answer. Past it the window is
/// assumed: the answer is an improvement on a session that would run without
/// it, not a reason to hold the session.
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// What one model's provider said: its window, or why it said none.
type Reported = Result<u32, String>;

/// The models a session has asked about, and what each provider said.
///
/// Shared by the session and its children, so a model is asked about once a
/// session, whichever agent first names it.
pub(crate) struct Windows {
    timeout: Duration,
    /// By `[models.<name>]` row, which names one provider and identifier. A
    /// row's cell is filled by the first to ask, and awaited by any that ask
    /// while it is: the map is locked only to find the cell, so a request in
    /// flight holds up no other row.
    asked: Mutex<HashMap<String, Arc<OnceCell<Reported>>>>,
}

impl Default for Windows {
    fn default() -> Self {
        Self::new(LOOKUP_TIMEOUT)
    }
}

impl Windows {
    /// A session that waits `timeout` for each provider.
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            asked: Mutex::new(HashMap::new()),
        }
    }

    /// Ask the provider of each of `candidates` that sets no `context-window`
    /// what its window is, all at once, unless this session already did. A
    /// model whose row sets one is not asked about.
    pub(crate) async fn fill(&self, candidates: &mut [ResolvedCandidate]) {
        join_all(
            candidates
                .iter_mut()
                .filter(|candidate| candidate.context_window.is_none())
                .map(|candidate| async {
                    let cell = Arc::clone(
                        self.asked
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .entry(candidate.model_name.clone())
                            .or_default(),
                    );
                    let reported = cell
                        .get_or_init(|| async {
                            let reported = lookup(candidate, self.timeout).await;
                            tracing::debug!(
                                model = %candidate.model_name,
                                reported = ?reported,
                                "asked the provider for the model's context window"
                            );
                            reported
                        })
                        .await;
                    candidate.reported_window = Some(reported.clone());
                }),
        )
        .await;
    }
}

/// The window `candidate`'s provider reports for its model, or why it reports
/// none. One request, bounded by `timeout`, and not retried.
///
/// A plain client rather than the model calls': those retry, and record each
/// attempt as a call the session made. It follows no redirect: a provider that
/// does not serve the route may send it anywhere, and the key rides in a
/// header (`x-api-key`) that a client carries across origins. A redirect is an
/// answer with no window, like any other status.
async fn lookup(candidate: &ResolvedCandidate, timeout: Duration) -> Reported {
    let http = http_client_builder(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| failed(e, timeout))?;
    let identifier = &candidate.model_identifier;
    let request = match &candidate.provider {
        ResolvedProvider::Anthropic {
            base_url, api_key, ..
        } => http
            .get(url(
                &normalize_anthropic_base_url(base_url),
                &["v1", "models", identifier],
            )?)
            .header("x-api-key", api_key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION_LATEST),
        ResolvedProvider::ChatCompletions {
            base_url, api_key, ..
        }
        | ResolvedProvider::Responses {
            base_url, api_key, ..
        } => http
            .get(url(base_url, &["model", "info"])?)
            .bearer_auth(api_key.expose()),
    };
    let response = request.send().await.map_err(|e| failed(e, timeout))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = response.bytes().await.map_err(|e| failed(e, timeout))?;
    let body: Value =
        serde_json::from_slice(&body).map_err(|_| "the reply is not JSON".to_string())?;
    match candidate.provider {
        ResolvedProvider::Anthropic { .. } => max_input_tokens(&body),
        ResolvedProvider::ChatCompletions { .. } | ResolvedProvider::Responses { .. } => {
            proxy_window(&body, identifier)
        }
    }
}

/// `base`, with `segments` added to its path. Never quotes `base`, which may
/// carry credentials.
fn url(base: &str, segments: &[&str]) -> Result<reqwest::Url, String> {
    const NOT_A_URL: &str = "the base-url is not a URL";
    let mut url = reqwest::Url::parse(base).map_err(|_| NOT_A_URL.to_string())?;
    url.path_segments_mut()
        .map_err(|()| NOT_A_URL.to_string())?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// `max_input_tokens` of a model object, as Anthropic's Models API answers
/// `GET /v1/models/{id}`.
fn max_input_tokens(model: &Value) -> Reported {
    match model["max_input_tokens"].as_u64() {
        Some(tokens) if tokens > 0 => {
            u32::try_from(tokens).map_err(|_| format!("reports a max_input_tokens of {tokens}"))
        }
        _ => Err("reports no max_input_tokens".to_string()),
    }
}

/// The window a LiteLLM proxy's `GET /model/info` lists for `identifier`: its
/// `model_info.max_input_tokens`. A proxy may list one name once per
/// deployment behind it, and a call can go to any, so the smallest is taken.
fn proxy_window(body: &Value, identifier: &str) -> Reported {
    let entries = body["data"]
        .as_array()
        .ok_or_else(|| "the reply lists no models".to_string())?;
    entries
        .iter()
        .filter(|entry| entry["model_name"] == identifier)
        .map(|entry| max_input_tokens(&entry["model_info"]))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or_else(|| format!("the reply lists no model {identifier:?}"))
}

/// Why a request did not get an answer, without quoting its URL.
fn failed(e: reqwest::Error, timeout: Duration) -> String {
    if e.is_timeout() {
        return format!("no answer in {timeout:?}");
    }
    let e = e.without_url();
    format!("the request failed: {}", innermost_cause(&e))
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod window_tests;
