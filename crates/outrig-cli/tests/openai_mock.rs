//! The `openai` provider arm against a local mock, for the one case its wire
//! shape decides on its own: a model that stopped with nothing to add.
//!
//! Not gated behind `e2e`: no podman, no network, no account. The agent is
//! built through the real `resolve_agent` -> `build_agent` path, so what the
//! mock sees is what an OpenAI-compatible gateway would.
//!
//! Why this needs its own file rather than a case in `anthropic_mock.rs`: the
//! two arms answer the *same* rig error for different reasons. rig's Anthropic
//! arm reads `stop_reason`, normalizes a documented empty `end_turn` into empty
//! text, and reaches the error only for real faults. Its OpenAI arm never reads
//! `finish_reason`, so a model that stopped and a response that was cut off are
//! indistinguishable by the time outrig sees anything. outrig therefore reads an
//! empty completion as an ending on this arm and as a failure on that one, and
//! only a test per arm can pin both halves.

mod common;

use std::net::SocketAddr;
use std::path::Path;

use serde_json::{Value, json};

use outrig::config::Config;
use outrig_cli::llm::{RigAgent, build_agent, resolve_agent};

use common::{CannedResponse, drain_recorded, set_test_env, start_mock_http, unset_test_env};

const KEY: &str = "sk-mock-not-a-real-key";
const MODEL: &str = "gpt-4o-mini";
const PREAMBLE: &str = "You are a test fixture.";

/// A chat-completions envelope carrying one choice. `content` is passed through
/// verbatim so a test can send `null`, `""`, or real text.
fn completion(content: Value, finish_reason: &str) -> CannedResponse {
    CannedResponse::ok(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL,
        "system_fingerprint": null,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "logprobs": null,
            "finish_reason": finish_reason,
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 0, "total_tokens": 12 },
    }))
}

fn mock_config(addr: SocketAddr, var: &str) -> Config {
    let cfg = format!(
        r#"
default-model = "mock"

[providers.gateway]
style                = "openai"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 10

[models.mock]
provider   = "gateway"
identifier = "{MODEL}"
max-tokens = 4096

[agents.coding]
preamble      = "{PREAMBLE}"
tool-call-max = 1
"#
    );
    let cfg = Config::load_from_str(&cfg).expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

async fn agent_for(addr: SocketAddr, var: &str) -> RigAgent {
    let cfg = &mock_config(addr, var);
    set_test_env(var, KEY);
    let resolved = resolve_agent(cfg, Some("coding")).expect("resolves");
    unset_test_env(var);

    #[cfg(feature = "local-llm")]
    let registry = std::sync::Arc::new(outrig_cli::llm::LlmRegistry::new());
    build_agent(
        &resolved,
        Vec::new(),
        Path::new("."),
        #[cfg(feature = "local-llm")]
        &registry,
    )
    .await
    .expect("the agent builds against the mock")
}

/// The behavior the whole change is for, observed end to end.
///
/// A gateway answering `200 OK` with an empty assistant message is a model that
/// stopped. It used to cost three identical model calls and end the turn with
/// "a response outrig could not use"; the agent speaks through its own channel
/// now, so a turn that sends and then stops is complete.
///
/// The request count is the assertion that matters -- both the old behavior and
/// the new one end the turn, and what separates them is what was spent.
#[tokio::test]
async fn a_model_that_stopped_ends_the_turn_on_the_first_call() {
    let (addr, mut requests) = start_mock_http(vec![
        completion(Value::Null, "stop"),
        // Never reached. Queued so that a regression retries into a *reply*
        // rather than into an exhausted mock, which would fail for the wrong
        // reason and read as a flake.
        completion(json!("recovered"), "stop"),
    ])
    .await;

    let agent = agent_for(addr, "OUTRIG_TEST_OPENAI_STOPPED").await;
    let mut history = Vec::new();
    let end = agent
        .run_turn("say nothing", &mut history)
        .await
        .expect("a model that stopped is not a turn failure");

    assert!(
        end.stopped.is_none(),
        "an ending is not a stop: {:?}",
        end.stopped,
    );
    assert!(end.reply.is_empty(), "got: {:?}", end.reply);
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "resending the same conversation only asks the model to stop again",
    );
}

/// An empty *string* is the same thing: rig drops a present-but-empty text part
/// (`providers/openai/completion/mod.rs:1146`), so it decodes to nothing at all
/// and is indistinguishable from an absent one.
#[tokio::test]
async fn an_empty_string_reply_is_the_same_ending() {
    let (addr, mut requests) = start_mock_http(vec![completion(json!(""), "stop")]).await;

    let agent = agent_for(addr, "OUTRIG_TEST_OPENAI_EMPTY_STR").await;
    let mut history = Vec::new();
    let end = agent
        .run_turn("say nothing", &mut history)
        .await
        .expect("an empty string is a model that stopped");

    assert!(end.stopped.is_none(), "got: {:?}", end.stopped);
    assert_eq!(drain_recorded(&mut requests).len(), 1);
}

/// The other side of the narrowing: an ordinary reply still ends the turn the
/// way it always did, so the change cannot be mistaken for "every turn is now
/// empty".
#[tokio::test]
async fn an_ordinary_reply_is_untouched() {
    let (addr, _requests) = start_mock_http(vec![completion(json!("Two crates."), "stop")]).await;

    let agent = agent_for(addr, "OUTRIG_TEST_OPENAI_REPLY").await;
    let mut history = Vec::new();
    let end = agent
        .run_turn("how many crates?", &mut history)
        .await
        .expect("an ordinary turn succeeds");

    assert_eq!(end.reply, "Two crates.");
    assert!(end.stopped.is_none());
    assert!(!end.is_silent(), "a turn with a reply is not silent");
}
