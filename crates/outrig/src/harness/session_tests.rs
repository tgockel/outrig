//! A session with no primary agent, on the host: what it publishes, what it asks of nobody, what
//! it refuses, and how it stops. The e2e module at the end starts one in a container.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::event::{Event, Payload, Received, SessionState, Subscription};
use super::{DEFAULT_DRAIN, Session, SessionBuilder, SessionError, Stopped, Verdict};
use crate::agent::mock_http::{self, MODEL};
use crate::config::{Config, Secrets};
use crate::python::testing::within;

const VAR: &str = "OUTRIG_TEST_NO_PRIMARY_KEY";

/// Where no model is served: a session that called one would fail its call.
const NOWHERE: &str = "127.0.0.1:9";

/// A configuration whose one model is served at `addr`, and is the default only with `default`,
/// and whose one agent, `coding`, runs that model and names no image.
fn config(addr: impl std::fmt::Display, default: bool) -> Config {
    let default = if default {
        r#"default-model = "sonnet""#
    } else {
        ""
    };
    let cfg = Config::load_from_str(&format!(
        r#"
{default}

[providers.claude]
style    = "anthropic"
base-url = "http://{addr}"
api-key  = "${{{VAR}}}"

[models.sonnet]
provider   = "claude"
identifier = "{MODEL}"

[agents.coding]
model = "sonnet"
"#
    ))
    .expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// A resolver that has every key, and counts how often it is asked for one.
fn counting() -> (impl Secrets + Clone + 'static, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let secrets = move |_: &str| {
        counter.fetch_add(1, Ordering::SeqCst);
        Some("sk-test".to_string())
    };
    (secrets, asked)
}

async fn start(builder: SessionBuilder) -> Session {
    within(builder.start_on_host())
        .await
        .unwrap_or_else(|e| panic!("the session did not start: {e}"))
}

/// Every event `events` held, in order, once its session has reported.
fn received(events: &mut Subscription) -> Vec<Arc<Event>> {
    let mut seen = Vec::new();
    while let Ok(received) = events.try_recv() {
        match received {
            Received::Event(event) => seen.push(event),
            Received::Missed(missed) => panic!("the subscription missed {missed} events"),
        }
    }
    seen
}

/// Each state `events` published, in order.
fn states(events: &[Arc<Event>]) -> Vec<SessionState> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::SessionState { state } => Some(*state),
            _ => None,
        })
        .collect()
}

/// A session with no primary agent is idle from its start until it closes: it resolves no model,
/// asks the resolver for nothing and calls no model, and runs no round, having no user channel and
/// no interrupter. Its report says it stopped, with no execution in it, and its events name no
/// agent: nothing says an agent started or stopped.
#[tokio::test]
async fn a_session_with_no_primary_agent_runs_no_round_and_calls_no_model() {
    let (addr, mut requests) = mock_http::start(Vec::new()).await;
    let (secrets, asked) = counting();
    let mut builder = SessionBuilder::new(config(addr, true), None, None)
        .secrets(secrets)
        .no_primary();
    let mut events = builder.subscribe();
    let mut session = start(builder).await;

    assert!(
        matches!(within(session.round()).await, Err(SessionError::NoPrimary)),
        "a round ran"
    );
    assert!(session.user_channel().is_none(), "a user channel");
    assert!(session.interrupter().is_none(), "an interrupter");
    assert_eq!(session.model(), None);
    assert_eq!(session.stop_held(), None);
    // The interpreter's own, which it greeted with before any kernel was open.
    assert!(
        session.python_version().starts_with("3."),
        "{}",
        session.python_version()
    );

    let report = within(session.shutdown(DEFAULT_DRAIN)).await;
    assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert!(report.executions.is_empty(), "{report:?}");
    assert!(report.bindings.is_none(), "{report:?}");

    let events = received(&mut events);
    assert_eq!(
        states(&events),
        [
            SessionState::Starting,
            SessionState::Idle,
            SessionState::Closing,
            SessionState::Reported,
        ]
    );
    let kinds: Vec<&str> = events.iter().map(|event| event.kind()).collect();
    for absent in ["agent.started", "agent.stopped", "interpreter.diagnostic"] {
        assert!(!kinds.contains(&absent), "{absent} in {kinds:?}");
    }
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "session.report")
            .count(),
        1,
        "{kinds:?}"
    );
    let subjects: Vec<_> = events
        .iter()
        .filter_map(|event| event.subject.as_ref())
        .collect();
    assert!(subjects.is_empty(), "events of an agent: {subjects:?}");

    assert_eq!(asked.load(Ordering::SeqCst), 0, "the resolver was asked");
    assert!(
        mock_http::drain(&mut requests).is_empty(),
        "the model was called"
    );
}

/// A configuration with no `default-model`, under a builder that names neither an agent nor a
/// model, is enough for a session with no primary agent, where one with a primary would fail on
/// it. A model the builder names is still checked: a name that does not exist fails `check`,
/// before anything starts.
#[tokio::test]
async fn a_session_with_no_primary_agent_needs_no_model() {
    let cfg = config(NOWHERE, false);
    let builder = || {
        SessionBuilder::new(cfg.clone(), None, None)
            .secrets(counting().0)
            .no_primary()
    };
    builder().check().expect("nothing to resolve");
    let report = within(start(builder()).await.shutdown(DEFAULT_DRAIN)).await;
    assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");

    let named = SessionBuilder::new(cfg.clone(), None, Some("nope"))
        .secrets(counting().0)
        .no_primary();
    match named.check() {
        Err(SessionError::Resolve(failure)) => {
            assert!(failure.to_string().contains("nope"), "{failure}");
        }
        other => panic!("expected the model to fail to resolve, got {other:?}"),
    }
    let with_primary = SessionBuilder::new(cfg, None, None).secrets(counting().0);
    assert!(
        matches!(with_primary.check(), Err(SessionError::Resolve(_))),
        "a primary agent with no model resolved one"
    );
}

/// The agent a builder with no primary names still chooses its container's image, so one the
/// configuration does not hold fails `check` and the start before anything starts, as it would
/// with a primary, and without asking for a key. One it holds, though it names no image of its
/// own, starts as a session that names none.
#[tokio::test]
async fn a_session_with_no_primary_agent_refuses_an_agent_it_does_not_know() {
    let cfg = config(NOWHERE, false);
    let (secrets, asked) = counting();
    let builder = |agent| {
        SessionBuilder::new(cfg.clone(), Some(agent), None)
            .secrets(secrets.clone())
            .no_primary()
    };
    let started = within(builder("typo").start()).await.map(drop);
    for refused in [builder("typo").check(), started] {
        match refused {
            Err(SessionError::Resolve(failure)) => {
                let text = failure.to_string();
                assert!(
                    text.contains("\"typo\" is not defined") && text.contains("coding"),
                    "{text}"
                );
            }
            other => panic!("expected the agent to fail to resolve, got {other:?}"),
        }
    }
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the resolver was asked");

    builder("coding").check().expect("a configured agent");
    let report = within(start(builder("coding")).await.shutdown(DEFAULT_DRAIN)).await;
    assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::LaunchSpec;
    use crate::python::testing::{ALPINE, pull};

    /// In a container, a session with no primary agent starts its interpreter with no kernel,
    /// is idle, and stops proven, having asked the resolver for nothing.
    #[tokio::test]
    async fn a_session_with_no_primary_agent_runs_in_a_container() {
        pull(ALPINE).await;
        let logs = tempfile::tempdir().expect("a log dir");
        let (secrets, asked) = counting();
        let mut builder = SessionBuilder::new(config(NOWHERE, true), None, None)
            .secrets(secrets)
            .container(LaunchSpec::from_image(
                ALPINE,
                BTreeMap::new(),
                logs.path().to_path_buf(),
            ))
            .no_primary();
        let mut events = builder.subscribe();
        let session = within(builder.start())
            .await
            .unwrap_or_else(|e| panic!("the session did not start: {e}"));
        assert_eq!(session.container_name(), format!("outrig-{}", session.id()));
        assert_eq!(session.model(), None);

        let report = within(session.shutdown(DEFAULT_DRAIN)).await;
        assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
        assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
        assert_eq!(
            states(&received(&mut events)),
            [
                SessionState::Starting,
                SessionState::Idle,
                SessionState::Closing,
                SessionState::Reported,
            ]
        );
        assert_eq!(asked.load(Ordering::SeqCst), 0, "the resolver was asked");
    }
}
