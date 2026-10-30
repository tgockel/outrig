//! The OpenAI-compatible provider, end to end against a local mock endpoint.
//!
//! Not gated behind `e2e`, for the reasons `anthropic_mock.rs` gives: the agent
//! is built through the real `resolve_agent` -> `build_agent` path, so the
//! requests observed here are the ones rig's OpenAI client would send to a real
//! endpoint, and nothing here needs podman, a network, or an account.
//!
//! What this pins:
//!
//! * a turn that produced only reasoning stays in outrig's history as rig
//!   returned it, and the next request still alternates roles: rig leaves a
//!   turn with nothing to send out of a Chat request and merges the prompts
//!   on either side of it, and sends a hand-built one -- a chain's shared
//!   history -- as the turn's text;
//! * reasoning beside text goes back under `reasoning_content`, as it came;
//! * a tool result goes out as the tool returned it, byte for byte, even when
//!   it is JSON that a reader could take for structure.

mod common;

use std::net::SocketAddr;

use rig::completion::Message;
use rig::completion::message::{AssistantContent, AssistantMessage};
use serde_json::{Value, json};

use outrig::config::Config;
use outrig_cli::llm::RigAgent;
use outrig_cli::session_tool;

use common::{CannedResponse, FIXED_TOOL, FixedTool, drain_recorded, start_mock_http};

// ---- canned chat completions ----------------------------------------------

const KEY: &str = "sk-mock-key";
const PREAMBLE: &str = "You are a careful coding assistant.";
const MODEL: &str = "gpt-4o";
const REASONING: &str = "Both call sites pass a borrowed path, so the helper can take &Path.";

/// A chat completion whose one choice is `message`, finishing for
/// `finish_reason`.
fn completion(message: Value, finish_reason: &str) -> CannedResponse {
    CannedResponse::ok(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": MODEL,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 },
    }))
}

/// The common case: one reply, turn over.
fn text_reply(text: &str) -> CannedResponse {
    completion(json!({ "role": "assistant", "content": text }), "stop")
}

// ---- harness --------------------------------------------------------------

/// A config running `model` against the mock with retries off: no test here is
/// about them, and a failure should end the turn at once.
///
/// `model` is `"gpt"`, a lone model, or `"chain"`, an alias over two rows on the
/// same mock. `build_agent` builds that into a `FailoverModel`, candidate by
/// candidate, apart from the path a lone model takes; a move along it would
/// show as a second request.
fn mock_config(addr: SocketAddr, var: &str, model: &str) -> Config {
    let cfg = format!(
        r#"
default-model = "{model}"

[providers.mock]
style                = "openai"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 10
retry-budget-secs    = 0

[models.gpt]
provider   = "mock"
identifier = "{MODEL}"

[models.gpt-fallback]
provider   = "mock"
identifier = "{MODEL}"

[models.chain]
alias = ["gpt", "gpt-fallback"]

[agents.coding]
preamble = "{PREAMBLE}"
"#
    );
    let cfg = Config::load_from_str(&cfg).expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// The agent under test: [`mock_config`]'s, with no tools.
async fn mock_agent(addr: SocketAddr, var: &str, model: &str) -> RigAgent {
    common::build_mock_agent(&mock_config(addr, var, model), KEY, &[var], Vec::new()).await
}

/// The turn a reasoning model stopped at its output ceiling leaves in history:
/// reasoning, and nothing else.
fn reasoning_only_turn() -> Message {
    Message::Assistant(AssistantMessage::new(vec![AssistantContent::reasoning(
        REASONING,
    )]))
}

// ---- tests ----------------------------------------------------------------

/// A turn that produced only reasoning is kept, and the next request still
/// alternates roles.
///
/// The OpenAI sibling of `anthropic_mock.rs`'s
/// `a_reasoning_only_turn_is_recovered_and_reported`: a reasoning model that
/// ended its turn having only reasoned. A Chat message needs content or calls,
/// so rig leaves the turn out of the next request -- as it always did -- and
/// merges the prompts on either side of it into one user message. That is
/// what keeps an endpoint that requires alternating roles from refusing the
/// request with a `400`, which used to end `outrig run`.
#[tokio::test]
async fn a_reasoning_only_turn_merges_the_prompts_around_it() {
    let (addr, mut requests) = start_mock_http(vec![
        completion(
            json!({ "role": "assistant", "content": null, "reasoning_content": REASONING }),
            "stop",
        ),
        text_reply("Not yet."),
    ])
    .await;
    let agent = mock_agent(addr, "OUTRIG_TEST_OPENAI_REASONING_ONLY", "gpt").await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("Can the helper take a &Path?", &mut history)
        .await
        .expect("a reasoning-only turn ends the turn, it does not fail the session");
    assert!(end.is_silent(), "no text and no stop reason: {end:?}");
    assert_eq!(
        end.recovered.as_deref(),
        Some(REASONING),
        "the reasoning is salvaged, as on the Anthropic arm",
    );

    agent
        .run_turn("Did you change anything?", &mut history)
        .await
        .expect("the next prompt is answered");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 2, "one request per turn");
    let messages = recorded[1].messages();
    assert_eq!(
        recorded[1].roles(),
        ["system", "user"],
        "the turn has nothing a Chat message can carry, so the prompts on either \
         side of it are one user message: {messages:#?}",
    );
    assert_eq!(
        messages[1]["content"],
        json!([
            { "type": "text", "text": "Can the helper take a &Path?" },
            { "type": "text", "text": "Did you change anything?" },
        ]),
        "{messages:#?}",
    );

    let Message::Assistant(turn) = &history[1] else {
        panic!("the turn is the assistant's: {history:#?}");
    };
    assert!(
        matches!(
            turn.content.as_slice(),
            [AssistantContent::Reasoning(reasoning)] if reasoning.text == REASONING
        ),
        "outrig's history keeps the turn as rig returned it: {turn:#?}",
    );
}

/// Reasoning beside text goes back under the field it came in.
#[tokio::test]
async fn reasoning_beside_text_goes_back_as_before() {
    let (addr, mut requests) = start_mock_http(vec![
        completion(
            json!({ "role": "assistant", "content": "It can.", "reasoning_content": REASONING }),
            "stop",
        ),
        text_reply("Not yet."),
    ])
    .await;
    let agent = mock_agent(addr, "OUTRIG_TEST_OPENAI_REASONING_BESIDE_TEXT", "gpt").await;

    let mut history = Vec::new();
    for prompt in ["Can the helper take a &Path?", "Did you change anything?"] {
        agent
            .run_turn(prompt, &mut history)
            .await
            .expect("each prompt is answered");
    }

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 2, "one request per turn");
    let messages = recorded[1].messages();
    assert_eq!(
        recorded[1].roles(),
        ["system", "user", "assistant", "user"],
        "{messages:#?}",
    );
    assert_eq!(
        messages[2],
        json!({
            "role": "assistant",
            "content": "It can.",
            "reasoning_content": REASONING,
        }),
    );
}

/// An alias chain's OpenAI candidate is sent a reasoning-only turn too.
///
/// The history holds the turn as a hand-built one, with no record of which
/// provider wrote it: reasoning with no provider item to replay goes out as
/// the turn's text, since there is no field on this dialect to carry it, and
/// the model still learns it took a turn.
#[tokio::test]
async fn a_chain_candidate_is_sent_a_reasoning_only_turn() {
    let (addr, mut requests) = start_mock_http(vec![text_reply("Not yet.")]).await;
    let agent = mock_agent(addr, "OUTRIG_TEST_OPENAI_CHAIN_REASONING_ONLY", "chain").await;

    let mut history = vec![
        Message::user("Can the helper take a &Path?"),
        reasoning_only_turn(),
    ];
    agent
        .run_turn("Did you change anything?", &mut history)
        .await
        .expect("the head takes the turn");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        recorded.len(),
        1,
        "the head answered, and the chain never moved",
    );
    let messages = recorded[0].messages();
    assert_eq!(
        recorded[0].roles(),
        ["system", "user", "assistant", "user"],
        "{messages:#?}",
    );
    assert_eq!(messages[2]["content"], REASONING, "{messages:#?}");
}

/// A tool result goes out as the tool returned it, even when it is JSON that
/// a reader could take for structure.
///
/// A tool's text is never meant as structure, and rig once re-parsed it as
/// such (#253): one with a top-level `response` key went out as that value
/// alone, so a stub mapping read from a file lost the `request` it matches.
/// This pins that nothing between the tool and the wire does.
#[tokio::test]
async fn a_json_tool_result_reaches_the_model_as_written() {
    let var = "OUTRIG_TEST_OPENAI_RESULT_AS_WRITTEN";
    for output in common::RESULTS_RIG_RESHAPES {
        let (addr, mut requests) = start_mock_http(vec![
            completion(
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": FIXED_TOOL, "arguments": "{}" },
                    }],
                }),
                "tool_calls",
            ),
            text_reply("Read it."),
        ])
        .await;
        let cfg = mock_config(addr, var, "gpt");
        let tools = session_tool::erase([FixedTool { output }]);
        let agent = common::build_mock_agent(&cfg, KEY, &[var], tools).await;

        let mut history = Vec::new();
        agent
            .run_turn("Run the tool.", &mut history)
            .await
            .unwrap_or_else(|err| panic!("the turn failed on {output}: {err}"));

        let recorded = drain_recorded(&mut requests);
        assert_eq!(recorded.len(), 2, "the tool call, then its result");
        let messages = recorded[1].messages();
        assert_eq!(
            recorded[1].roles(),
            ["system", "user", "assistant", "tool"],
            "{messages:#?}",
        );
        assert_eq!(
            messages[3]["content"], output,
            "the model reads what the tool returned, byte for byte",
        );
    }
}
