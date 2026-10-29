//! A model's window, asked of a scripted provider.
//!
//! What is asked is read off the requests the mock recorded; what becomes of
//! the answer, off the budget built from the candidate it was left on.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

use super::Windows;
use crate::agent::agent_tests::KEY;
use crate::agent::budget::{self, ASSUMED_CONTEXT_WINDOW, Budget};
use crate::agent::mock_http::{self, CannedResponse, Style};
use crate::agent::resolve::{ResolvedCandidate, ResolvedProvider};
use crate::config::RoleAlternation;
use crate::harness::event::WindowSource;

const IDENTIFIER: &str = "claude-sonnet-5";

/// The model `IDENTIFIER`, row `sonnet`, on a provider of `style` at
/// `base_url`, with `context_window` on its row.
fn candidate_on(style: Style, base_url: String, context_window: Option<u32>) -> ResolvedCandidate {
    let (request_timeout_secs, retry_budget_secs, role_alternation) =
        (None, None, RoleAlternation::Relaxed);
    let provider = match style {
        Style::Anthropic => ResolvedProvider::Anthropic {
            base_url,
            api_key: KEY.into(),
            request_timeout_secs,
            retry_budget_secs,
            role_alternation,
        },
        Style::OpenAi => ResolvedProvider::OpenAi {
            base_url,
            api_key: KEY.into(),
            request_timeout_secs,
            retry_budget_secs,
            role_alternation,
        },
    };
    ResolvedCandidate {
        model_identifier: IDENTIFIER.to_string(),
        provider,
        ..budget::candidate("sonnet", context_window)
    }
}

/// The base URL a config would give a provider of `style` at `addr`: an
/// OpenAI-style one names the API's `/v1`.
fn base(style: Style, addr: SocketAddr) -> String {
    match style {
        Style::Anthropic => format!("http://{addr}"),
        Style::OpenAi => format!("http://{addr}/v1"),
    }
}

/// Ask for `candidate`'s window as a session that has asked nothing yet.
async fn ask(candidate: &mut ResolvedCandidate) {
    Windows::default()
        .fill(std::slice::from_mut(candidate))
        .await;
}

/// The budget `candidate`'s model gets, as the session builds it.
fn budget(candidate: &ResolvedCandidate) -> Budget {
    Budget::new(candidate, Some(4096), 100).expect("room for a round")
}

/// Asking, and what is asked: a `GET` of the model, which carries the key as a
/// call to it does.
#[tokio::test]
async fn a_provider_is_asked_for_the_window_of_the_model_it_serves() {
    for style in Style::ALL {
        let (addr, mut requests) =
            mock_http::start_reporting(vec![], style.window(IDENTIFIER, Some(1_000_000))).await;
        let mut candidate = candidate_on(style, base(style, addr), None);
        ask(&mut candidate).await;

        assert_eq!(candidate.reported_window, Some(Ok(1_000_000)), "{style:?}");
        let budget = budget(&candidate);
        assert_eq!(
            (budget.window, budget.window_source),
            (1_000_000, WindowSource::Provider),
            "{style:?}"
        );

        let asked = mock_http::drain(&mut requests);
        assert_eq!(asked.len(), 1, "{style:?}: asked once");
        assert_eq!(asked[0].method, "GET", "{style:?}");
        match style {
            Style::Anthropic => {
                assert_eq!(asked[0].path, format!("/v1/models/{IDENTIFIER}"));
                assert_eq!(asked[0].header("x-api-key"), Some(KEY));
                assert!(asked[0].header("anthropic-version").is_some());
            }
            Style::OpenAi => {
                assert_eq!(asked[0].path, "/v1/model/info");
                assert_eq!(
                    asked[0].header("authorization"),
                    Some(format!("Bearer {KEY}").as_str())
                );
            }
        }
    }
}

/// An Anthropic base URL may carry the path a call to it adds, which rig
/// strips; the lookup takes the same address a call does.
#[tokio::test]
async fn an_anthropic_base_url_is_normalized_as_a_call_to_it_is() {
    for suffix in ["", "/", "/v1", "/v1/", "/v1/messages"] {
        let (addr, mut requests) =
            mock_http::start_reporting(vec![], Style::Anthropic.window(IDENTIFIER, Some(200_000)))
                .await;
        let mut candidate = candidate_on(Style::Anthropic, format!("http://{addr}{suffix}"), None);
        ask(&mut candidate).await;

        assert_eq!(candidate.reported_window, Some(Ok(200_000)), "{suffix:?}");
        let asked = mock_http::drain(&mut requests);
        assert_eq!(
            asked[0].path,
            format!("/v1/models/{IDENTIFIER}"),
            "{suffix:?}"
        );
    }
}

/// A proxy lists a model once for each deployment behind it, and a call can go
/// to any, so the smallest window is the one it can be held to.
#[tokio::test]
async fn a_proxy_that_lists_a_model_twice_is_held_to_the_smaller_window() {
    let listing = mock_http::proxy_listing(&[
        (IDENTIFIER, Some(1_000_000)),
        ("other", Some(8_000)),
        (IDENTIFIER, Some(229_376)),
    ]);
    let (addr, _requests) = mock_http::start_reporting(vec![], listing).await;
    let mut candidate = candidate_on(Style::OpenAi, base(Style::OpenAi, addr), None);
    ask(&mut candidate).await;

    assert_eq!(candidate.reported_window, Some(Ok(229_376)));
}

/// Every way the provider can fail to say leaves the model on the assumed
/// window, with the reason it was not reported, and the budget builds: nothing
/// here fails a session's start.
#[tokio::test]
async fn a_provider_that_reports_no_window_leaves_the_assumption_and_says_why() {
    for style in Style::ALL {
        let mut cases: Vec<(&str, CannedResponse, String)> = vec![
            ("not found", style.failure(404), "HTTP 404".to_string()),
            ("forbidden", style.failure(403), "HTTP 403".to_string()),
            (
                "a page, not JSON",
                mock_http::not_json(),
                "the reply is not JSON".to_string(),
            ),
            (
                "a null window",
                style.window(IDENTIFIER, None),
                "reports no max_input_tokens".to_string(),
            ),
            (
                "a window of nothing",
                style.window(IDENTIFIER, Some(0)),
                "reports no max_input_tokens".to_string(),
            ),
        ];
        if style == Style::OpenAi {
            cases.push((
                "another model's row",
                style.window("other", Some(8_000)),
                format!("the reply lists no model {IDENTIFIER:?}"),
            ));
            cases.push((
                "a completion, not a listing",
                style.text("hi"),
                "the reply lists no models".to_string(),
            ));
        }
        for (what, reply, why) in cases {
            let (addr, _requests) = mock_http::start_reporting(vec![], reply).await;
            let mut candidate = candidate_on(style, base(style, addr), None);
            ask(&mut candidate).await;

            assert_eq!(
                candidate.reported_window,
                Some(Err(why)),
                "{style:?}: {what}"
            );
            let budget = budget(&candidate);
            assert_eq!(
                (budget.window, budget.window_source),
                (ASSUMED_CONTEXT_WINDOW, WindowSource::Assumed),
                "{style:?}: {what}"
            );
        }
    }
}

/// A provider that sends the lookup somewhere else is not followed: the key it
/// carries goes to the provider it was configured for, and no other.
#[tokio::test]
async fn a_redirect_is_not_followed_with_the_providers_key() {
    for style in Style::ALL {
        let (elsewhere, mut elsewhere_requests) =
            mock_http::start_reporting(vec![], style.window(IDENTIFIER, Some(1_000_000))).await;
        let to = format!("http://{elsewhere}/v1/models/{IDENTIFIER}");
        let (addr, mut requests) =
            mock_http::start_reporting(vec![], mock_http::redirect(&to)).await;
        let mut candidate = candidate_on(style, base(style, addr), None);
        ask(&mut candidate).await;

        assert_eq!(
            candidate.reported_window,
            Some(Err("HTTP 302".to_string())),
            "{style:?}"
        );
        assert_eq!(budget(&candidate).window_source, WindowSource::Assumed);
        assert_eq!(mock_http::drain(&mut requests).len(), 1, "{style:?}");
        assert!(
            mock_http::drain(&mut elsewhere_requests).is_empty(),
            "{style:?}: the redirect was followed"
        );
    }
}

/// A provider that never answers is waited for only so long, and one that is
/// not there is not waited for at all.
#[tokio::test]
async fn a_provider_that_does_not_answer_is_not_waited_for() {
    for style in Style::ALL {
        // Accepts the connection, in the backlog, and reads nothing.
        let silent = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
        let addr = silent.local_addr().expect("its address");
        let mut candidate = candidate_on(style, base(style, addr), None);
        let started = Instant::now();
        Windows::new(Duration::from_millis(200))
            .fill(std::slice::from_mut(&mut candidate))
            .await;

        assert!(started.elapsed() < Duration::from_secs(5), "{style:?}");
        assert_eq!(
            candidate.reported_window,
            Some(Err("no answer in 200ms".to_string())),
            "{style:?}"
        );
        assert_eq!(budget(&candidate).window_source, WindowSource::Assumed);

        let refused = format!("http://127.0.0.1:{}", mock_http::REFUSED_PORT);
        let mut candidate = candidate_on(style, refused, None);
        ask(&mut candidate).await;
        assert_eq!(
            candidate.reported_window,
            Some(Err(format!(
                "the request failed: {}",
                mock_http::refused_cause().await
            ))),
            "{style:?}"
        );
        assert_eq!(budget(&candidate).window_source, WindowSource::Assumed);
    }
}

/// An alias's models are asked at once, so providers that do not answer cost
/// one wait between them, not one each.
#[tokio::test]
async fn the_models_of_an_alias_are_asked_at_once() {
    let first = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
    let second = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
    let mut candidates = [
        candidate_on(
            Style::Anthropic,
            base(Style::Anthropic, first.local_addr().expect("its address")),
            None,
        ),
        candidate_on(
            Style::Anthropic,
            base(Style::Anthropic, second.local_addr().expect("its address")),
            None,
        ),
    ];
    candidates[1].model_name = "other".to_string();
    let started = Instant::now();
    Windows::new(Duration::from_millis(500))
        .fill(&mut candidates)
        .await;

    // Asked one after the other, the two waits would take a second.
    assert!(started.elapsed() < Duration::from_millis(900));
    for candidate in &candidates {
        assert_eq!(
            candidate.reported_window,
            Some(Err("no answer in 500ms".to_string()))
        );
    }
}

/// A row that sets a window is not asked, whatever its provider would say; and
/// a model another agent names is not asked about again.
#[tokio::test]
async fn a_model_is_asked_about_only_when_its_row_is_silent_and_only_once() {
    for style in Style::ALL {
        let (addr, mut requests) =
            mock_http::start_reporting(vec![], style.window(IDENTIFIER, Some(1_000_000))).await;
        let windows = Windows::default();

        let mut set = candidate_on(style, base(style, addr), Some(200_000));
        windows.fill(std::slice::from_mut(&mut set)).await;
        assert_eq!(set.reported_window, None, "{style:?}");
        assert!(mock_http::drain(&mut requests).is_empty(), "{style:?}");
        assert_eq!(budget(&set).window_source, WindowSource::Configured);

        let mut first = candidate_on(style, base(style, addr), None);
        windows.fill(std::slice::from_mut(&mut first)).await;
        assert_eq!(mock_http::drain(&mut requests).len(), 1, "{style:?}");

        let mut second = candidate_on(style, base(style, addr), None);
        windows.fill(std::slice::from_mut(&mut second)).await;
        assert_eq!(second.reported_window, Some(Ok(1_000_000)), "{style:?}");
        assert!(
            mock_http::drain(&mut requests).is_empty(),
            "{style:?}: the same model is not asked about again"
        );
    }
}
