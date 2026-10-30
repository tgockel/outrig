//! The native Anthropic provider, end to end against a local mock endpoint.
//!
//! Not gated behind `e2e`: nothing here needs podman, a network, or a paid
//! Anthropic account. The agent is built through the real
//! `resolve_agent` -> `build_agent` path, so the requests observed here are
//! the ones rig's Anthropic client would send to `api.anthropic.com` -- the
//! only substitution is the base URL and a fake key. The registered tool runs
//! in-process, which is what lets a full `tool_use` -> tool -> `tool_result`
//! round trip run in a unit-test-shaped test.
//!
//! What this pins that no other test can:
//!
//! * the wire shape is Anthropic's, not an OpenAI-compatible one (path,
//!   `x-api-key`, `anthropic-version`, `input_schema`, content blocks);
//! * a tool result goes back as the tool returned it, byte for byte, even
//!   when it is JSON that a reader could take for structure;
//! * which of the three `max_tokens` tiers -- config, rig's published ceiling,
//!   OutRig's fallback -- reaches the wire for a model identifier rig
//!   recognizes and one it does not;
//! * every request asks for prompt caching, which rig leaves off unless asked;
//! * the shared retry client covers this provider too -- including that a
//!   `Retry-After` sets the wait, that a spent budget ends the turn without
//!   taking the session with it, and that a redirect loop, which no wait can
//!   clear, is final at once;
//! * a `200` carrying no usable content is retried above the HTTP client,
//!   which is the only place it is visible at all, and ends the turn rather
//!   than the session when it persists;
//! * an alias chain moves to its next candidate *inside* a `completion()`
//!   call, so a turn that has already run a tool keeps its history and does
//!   not re-execute it -- which needs two endpoints, and so needs two mocks;
//! * a turn that stops past its first model call without finishing -- a call
//!   that failed for good, or a future dropped the way Ctrl-C drops it --
//!   keeps the tool calls it ran, answered in the order Anthropic requires,
//!   for the next request to carry;
//! * reasoning Anthropic did not issue -- a fallback's, left in a chain's
//!   shared history -- is not sent back to it as a `thinking` block it would
//!   refuse, while a turn that was only its own thinking goes back as the
//!   signed block it came as.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rig::completion::Message;
use rig::completion::message::{
    AssistantContent, AssistantMessage, Origin, Reasoning, ToolResultContent, UserContent,
};
use rig::tool::{ToolContext, ToolExecutionError, ToolOutput};
use serde_json::{Value, json};

use outrig::config::Config;
use outrig_cli::llm::{RigAgent, TurnStop};
use outrig_cli::session_tool::{self, OutrigTool};

use common::{
    CannedResponse, FIXED_TOOL, FixedTool, RecordedRequest, drain_recorded, start_mock_http,
};

// ---- the in-process tool --------------------------------------------------

/// A tool with no dependencies beyond itself, so a tool round trip needs no
/// container. Counts its calls so a test can tell "the model asked for this"
/// from "the tool ran".
#[derive(Clone, Default)]
struct EchoTool {
    calls: Arc<AtomicUsize>,
}

impl EchoTool {
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl OutrigTool for EchoTool {
    fn name(&self) -> String {
        "outrig_test_echo".to_string()
    }

    fn description(&self) -> String {
        "Echo the `value` argument back, prefixed with `pong:`.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string", "description": "Text to echo." }
            },
            "required": ["value"],
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let value = args
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(ToolOutput::text(format!("pong:{value}")))
    }
}

// ---- canned Anthropic responses -------------------------------------------

const KEY: &str = "sk-ant-mock-key";
const PREAMBLE: &str = "You are a careful coding assistant.";
/// Low enough that a test can drive the agent into the cap in one turn.
const TOOL_CALL_MAX: usize = 1;
const MODEL: &str = "claude-sonnet-4-6";
/// The output-token ceiling rig's catalog publishes for [`MODEL`].
const MODEL_CEILING: u64 = 128_000;
/// A recognized identifier whose published ceiling differs from both `MODEL`'s
/// and outrig's fallback, so a test can tell the three apart by the number
/// alone.
const OTHER_CEILING_MODEL: &str = "claude-haiku-4-5";
/// The output-token ceiling rig's catalog publishes for [`OTHER_CEILING_MODEL`].
const OTHER_CEILING: u64 = 64_000;
/// An identifier outrig has no published ceiling for -- which is the case a
/// user picking anything but a current model hits on their first run.
const UNRECOGNIZED_MODEL: &str = "claude-3-5-sonnet-20241022";

/// A message envelope in the shape rig's `ApiResponse` deserializes: the
/// `"type": "message"` tag plus every non-optional field of its
/// `CompletionResponse`.
fn message(content: Value, stop_reason: &str) -> CannedResponse {
    CannedResponse::ok(json!({
        "type": "message",
        "id": "msg_mock",
        "model": MODEL,
        "role": "assistant",
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "content": content,
        "usage": { "input_tokens": 12, "output_tokens": 7 },
    }))
}

/// The common case: one text block, turn over.
fn text_reply(text: &str) -> CannedResponse {
    message(json!([{ "type": "text", "text": text }]), "end_turn")
}

// ---- harness --------------------------------------------------------------

/// A config pointing an `anthropic` provider at the mock. `retry_budget_secs`
/// bounds -- or with `0`, switches off -- the transient-retry loop; `None`
/// leaves the shipped default, which is what a test not about retries wants.
fn mock_config(
    addr: SocketAddr,
    var: &str,
    identifier: &str,
    max_tokens: Option<u32>,
    retry_budget_secs: Option<u64>,
) -> Config {
    let max_tokens = max_tokens.map_or(String::new(), |n| format!("max-tokens = {n}"));
    let retry_budget =
        retry_budget_secs.map_or(String::new(), |n| format!("retry-budget-secs    = {n}"));
    let cfg = format!(
        r#"
default-model = "sonnet"

[providers.claude]
style                = "anthropic"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 10
{retry_budget}

[models.sonnet]
provider   = "claude"
identifier = "{identifier}"
{max_tokens}

[agents.coding]
preamble      = "{PREAMBLE}"
tool-call-max = {TOOL_CALL_MAX}
"#
    );
    let cfg = Config::load_from_str(&cfg).expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// Two provider-equivalent rows on two mocks, plus an alias naming them in
/// preference order -- the shape `build_agent` turns into a `FailoverModel`
/// rather than a lone retrying model.
///
/// Both providers pin `retry-budget-secs = 0`, which is how a test makes the
/// head give up at once: zero is its bound, so the move happens without
/// waiting out a backoff curve. It also makes the assertion about *when* the
/// move happens exact -- one failed request, not an unpredictable number.
fn mock_chain_config(head: SocketAddr, next: SocketAddr, vars: [&str; 2]) -> Config {
    let [head_var, next_var] = vars;
    let cfg = format!(
        r#"
default-model = "chain"

[providers.head]
style                = "anthropic"
base-url             = "http://{head}"
api-key              = "${{{head_var}}}"
request-timeout-secs = 10
retry-budget-secs    = 0

[providers.next]
style                = "anthropic"
base-url             = "http://{next}"
api-key              = "${{{next_var}}}"
request-timeout-secs = 10
retry-budget-secs    = 0

[models.chain]
alias = ["head-model", "next-model"]

[models.head-model]
provider   = "head"
identifier = "{MODEL}"
max-tokens = 4096

[models.next-model]
provider   = "next"
identifier = "{MODEL}"
max-tokens = 4096

[agents.coding]
preamble      = "{PREAMBLE}"
tool-call-max = {TOOL_CALL_MAX}
"#
    );
    let cfg = Config::load_from_str(&cfg).expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// Resolve and build an agent against the mock, then run one turn through it.
/// `var` is the test's own env-var name for the fake key -- unique per test,
/// which is what makes the `set_test_env` calls safe.
async fn run_one_turn(
    addr: SocketAddr,
    var: &str,
    identifier: &str,
    max_tokens: Option<u32>,
    tools: Vec<EchoTool>,
) -> anyhow::Result<String> {
    let cfg = mock_config(addr, var, identifier, max_tokens, None);
    let agent = build_mock_agent(&cfg, &[var], tools).await;
    let mut history = Vec::new();
    agent
        .run_turn("echo ping for me", &mut history)
        .await
        .map(|end| end.reply)
        .map_err(anyhow::Error::from)
}

/// Run one turn and hand back the `max_tokens` that reached the wire.
///
/// Every ceiling case is the same four steps -- stand up a mock, run a turn,
/// drain, assert exactly one request -- differing only in the identifier and the
/// configured ceiling, so the steps live here and each case is left as the two
/// numbers it is actually about.
async fn recorded_max_tokens(
    var: &str,
    identifier: &str,
    max_tokens: Option<u32>,
) -> serde_json::Value {
    let (addr, mut requests) = start_mock_http(vec![text_reply("ok")]).await;
    run_one_turn(addr, var, identifier, max_tokens, vec![])
        .await
        .expect("the turn must reach the provider for its ceiling to be observable");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1, "one turn is one request");
    recorded[0].body["max_tokens"].clone()
}

/// [`common::build_mock_agent`] with this file's key and its in-process tools.
/// Split out of [`run_one_turn`] so a test can drive more than one turn through
/// the same agent, which is what "the session survived" means.
async fn build_mock_agent(cfg: &Config, vars: &[&str], tools: Vec<EchoTool>) -> RigAgent {
    common::build_mock_agent(cfg, KEY, vars, session_tool::erase(tools)).await
}

// ---- tests ----------------------------------------------------------------

/// The whole native round trip: request shape and auth, a `tool_use` block
/// dispatched to an OutRig tool, and the `tool_result` continuation that
/// carries its output back.
#[tokio::test]
async fn native_tool_use_round_trip() {
    let (addr, mut requests) = start_mock_http(vec![
        message(
            json!([
                { "type": "text", "text": "Let me echo that." },
                {
                    "type": "tool_use",
                    "id": "toolu_mock_1",
                    "name": "outrig_test_echo",
                    "input": { "value": "ping" }
                },
            ]),
            "tool_use",
        ),
        text_reply("The echo said pong:ping."),
    ])
    .await;

    let tool = EchoTool::default();
    let reply = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_TOOL_USE",
        MODEL,
        Some(4096),
        vec![tool.clone()],
    )
    .await
    .expect("turn completes");

    assert_eq!(reply, "The echo said pong:ping.");
    assert_eq!(tool.call_count(), 1, "the model's tool_use should have run");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 2, "one call per model turn: {recorded:#?}");

    // --- request 1: the native path, auth, and body ---
    let first = &recorded[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/v1/messages");
    assert_eq!(first.header("x-api-key"), Some(KEY));
    assert_eq!(first.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        first.header("authorization"),
        None,
        "Anthropic authenticates with x-api-key; a Bearer header would mean \
         we are speaking the OpenAI-compatible shape by accident",
    );

    let body = &first.body;
    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(
        body["system"],
        json!([{
            "type": "text",
            "text": PREAMBLE,
            "cache_control": { "type": "ephemeral" },
        }]),
        "the preamble travels in Anthropic's top-level `system`, not as a \
         message with role=system",
    );
    assert!(
        body.get("stream").is_none(),
        "outrig's remote path is non-streaming: {body:#?}",
    );

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "one registered tool: {tools:#?}");
    assert_eq!(tools[0]["name"], "outrig_test_echo");
    assert_eq!(
        tools[0]["input_schema"]["properties"]["value"]["type"],
        "string"
    );
    assert!(
        tools[0].get("function").is_none() && tools[0].get("parameters").is_none(),
        "tools should use Anthropic's `input_schema` shape, not OpenAI's \
         `function`/`parameters`: {:#?}",
        tools[0],
    );

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");

    // --- request 2: the tool result goes back as a native block ---
    let second = &recorded[1];
    assert_eq!(second.path, "/v1/messages");
    let messages = second.body["messages"].as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        3,
        "user prompt, the assistant's tool_use, then its result: {messages:#?}",
    );

    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    let tool_use = assistant["content"]
        .as_array()
        .expect("assistant content array")
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("the assistant turn should carry the tool_use block back");
    assert_eq!(tool_use["id"], "toolu_mock_1");
    assert_eq!(tool_use["name"], "outrig_test_echo");

    let result_block = &messages[2]["content"][0];
    assert_eq!(
        messages[2]["role"], "user",
        "Anthropic carries tool results on a user turn",
    );
    assert_eq!(result_block["type"], "tool_result");
    assert_eq!(
        result_block["tool_use_id"], "toolu_mock_1",
        "the result must name the call it answers",
    );
    assert!(
        result_block.to_string().contains("pong:ping"),
        "the tool's output should reach the model: {result_block:#?}",
    );
}

/// A model's own `style` overrides its provider's: on a `chat-completions`
/// provider whose `base-url` ends in `/v1`, the model that says `anthropic`
/// posts to `/v1/messages` with `x-api-key`, and the one that says nothing to
/// `/v1/chat/completions` with a bearer token -- one endpoint, one key.
#[tokio::test]
async fn a_models_own_style_overrides_its_providers() {
    let var = "OUTRIG_TEST_ANTHROPIC_MODEL_STYLE";
    let chat_reply = CannedResponse::ok(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "over chat completions" },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19 },
    }));
    let (addr, mut requests) = start_mock_http(vec![chat_reply, text_reply("over messages")]).await;
    let config = |model: &str| {
        let cfg = Config::load_from_str(&format!(
            r#"
default-model = "{model}"

[providers.gateway]
style                = "chat-completions"
base-url             = "http://{addr}/v1"
api-key              = "${{{var}}}"
request-timeout-secs = 10

[models.gpt]
provider   = "gateway"
identifier = "gpt-4o"

[models.sonnet]
provider   = "gateway"
identifier = "{MODEL}"
style      = "anthropic"

[agents.coding]
preamble = "{PREAMBLE}"
"#
        ))
        .expect("config parses");
        cfg.validate(None).expect("config validates");
        cfg
    };

    for (model, reply) in [
        ("gpt", "over chat completions"),
        ("sonnet", "over messages"),
    ] {
        let agent = build_mock_agent(&config(model), &[var], Vec::new()).await;
        let end = agent
            .run_turn("hi", &mut Vec::new())
            .await
            .expect("turn completes");
        assert_eq!(end.reply, reply, "{model}");
    }

    let recorded = drain_recorded(&mut requests);
    let sent: Vec<_> = recorded
        .iter()
        .map(|r| {
            let auth = (r.header("authorization"), r.header("x-api-key"));
            (r.path.as_str(), auth)
        })
        .collect();
    let bearer = format!("Bearer {KEY}");
    assert_eq!(
        sent,
        [
            ("/v1/chat/completions", (Some(bearer.as_str()), None)),
            ("/v1/messages", (None, Some(KEY))),
        ]
    );
}

/// A provider's redirect is not followed, so the key goes nowhere but the
/// provider: `x-api-key` is a header reqwest keeps across origins, and a `307`
/// would send the conversation again. The turn fails at the provider, and the
/// address the redirect names is sent nothing.
#[tokio::test]
async fn a_providers_redirect_is_not_followed() {
    let (elsewhere, mut sent_elsewhere) = start_mock_http(vec![text_reply("leaked")]).await;
    let location = format!("http://{elsewhere}/v1/messages");
    for status in [302, 307] {
        let var = format!("OUTRIG_TEST_ANTHROPIC_REDIRECT_{status}");
        let (addr, mut requests) = start_mock_http(vec![
            CannedResponse::status(status, json!({})).with_header("Location", &location),
        ])
        .await;
        let err = run_one_turn(addr, &var, MODEL, Some(4096), vec![])
            .await
            .expect_err("a redirect fails the turn");
        assert!(
            format!("{err:#}").contains(&status.to_string()),
            "{status}: {err:#}"
        );
        assert_eq!(drain_recorded(&mut requests).len(), 1, "{status}");
    }
    let leaked = drain_recorded(&mut sent_elsewhere);
    assert!(leaked.is_empty(), "{leaked:#?}");
}

/// Every request asks Anthropic to cache, or no call is ever read back (#669).
///
/// Three markers, of the four a request may carry: one each on the last tool
/// and the last system block, the part every call shares, and one on the last
/// block of the last user message, which moves to the end of the request as
/// the history grows. Both requests of a tool round trip are held to it, since
/// the second is the one that has something to read back.
#[tokio::test]
async fn every_request_asks_for_prompt_caching() {
    let (addr, mut requests) = start_mock_http(vec![echo_tool_use(), text_reply("done")]).await;

    run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_PROMPT_CACHING",
        MODEL,
        Some(4096),
        vec![EchoTool::default()],
    )
    .await
    .expect("turn completes");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 2, "one call per model turn: {recorded:#?}");

    let ephemeral = json!({ "type": "ephemeral" });
    for (n, request) in recorded.iter().enumerate() {
        let body = &request.body;
        let last = request.messages().last().expect("a user message");
        assert_eq!(last["role"], "user", "request {n} ends in the user's turn");
        let last_block = last["content"]
            .as_array()
            .and_then(|blocks| blocks.last())
            .expect("content blocks");
        assert_eq!(
            last_block["cache_control"], ephemeral,
            "request {n}: the end of the request is the marker that moves: {body:#?}",
        );
        let system = body["system"].as_array().expect("system blocks");
        assert_eq!(
            system.last().expect("a system block")["cache_control"],
            ephemeral,
            "request {n}: the system prompt ends a cached prefix: {system:#?}",
        );
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(
            tools.last().expect("a tool")["cache_control"],
            ephemeral,
            "request {n}: the tool definition ends a cached prefix: {tools:#?}",
        );
        // Those three are all of them: no top-level marker, and none names a
        // TTL, so the default five minutes applies.
        assert_eq!(
            body.to_string().matches("\"cache_control\"").count(),
            3,
            "request {n}: no marker beyond those three: {body:#?}",
        );
    }
}

/// A tool result goes back as the tool returned it, even when it is JSON that
/// a reader could take for structure.
///
/// The Anthropic sibling of `openai_mock.rs`'s test of the same name. A tool's
/// text is never meant as structure, and rig once re-parsed it as such (#253);
/// this pins that nothing between the tool and the wire does.
#[tokio::test]
async fn a_json_tool_result_reaches_the_model_as_written() {
    let var = "OUTRIG_TEST_ANTHROPIC_RESULT_AS_WRITTEN";
    for output in common::RESULTS_RIG_RESHAPES {
        let (addr, mut requests) = start_mock_http(vec![
            tool_use(FIXED_TOOL, json!({})),
            text_reply("Read it."),
        ])
        .await;
        let cfg = mock_config(addr, var, MODEL, Some(4096), Some(0));
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
            ["user", "assistant", "user"],
            "{messages:#?}",
        );
        let result_block = &messages[2]["content"][0];
        assert_eq!(result_block["type"], "tool_result");
        assert_eq!(
            result_block["content"],
            json!([{ "type": "text", "text": output }]),
            "the model reads what the tool returned, byte for byte",
        );
    }
}

/// The per-turn tool-call cap governs this provider too. The cap lives in
/// `OutrigPromptHook`, which the Anthropic arm builds from
/// `resolved.tool_call_max` exactly as the OpenAI arm does -- but "exactly as"
/// is a claim about wiring that only a turn can check.
#[tokio::test]
async fn tool_call_cap_applies_to_the_anthropic_path() {
    let ask_for_echo = |id: &str| {
        message(
            json!([{
                "type": "tool_use",
                "id": id,
                "name": "outrig_test_echo",
                "input": { "value": "ping" }
            }]),
            "tool_use",
        )
    };
    let (addr, mut requests) = start_mock_http(vec![
        ask_for_echo("toolu_mock_1"),
        // The model asks a second time. The cap is 1, so this call is refused
        // and the turn ends before a third request is ever made -- a third
        // scripted reply would go unused, so there isn't one.
        ask_for_echo("toolu_mock_2"),
    ])
    .await;

    let tool = EchoTool::default();
    let reply = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_TOOL_CAP",
        MODEL,
        Some(4096),
        vec![tool.clone()],
    )
    .await
    .expect("the turn ends cleanly rather than erroring");

    assert_eq!(
        tool.call_count(),
        TOOL_CALL_MAX,
        "the second tool_use should have been refused by the cap, not run",
    );
    assert_eq!(
        reply,
        format!("(turn ended: tool-call iteration max ({TOOL_CALL_MAX}) reached)"),
        "the user is told why the turn stopped, in the shared wording",
    );

    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        recorded.len(),
        2,
        "the turn stops at the cap instead of making a third model call: {recorded:#?}",
    );
}

/// A retry-worthy status is retried by the shared HTTP client, one request at
/// a time, without replaying the turn.
///
/// Deliberately not `start_paused`: with paused time, tokio auto-advances the
/// clock whenever every task is parked -- including while the mock waits on a
/// socket read -- which can fire the request timeout before the response
/// lands. The real wait here is one jittered base delay, at most a second.
#[tokio::test]
async fn transient_status_is_retried_at_the_http_layer() {
    assert_retried_at_the_http_layer("OUTRIG_TEST_ANTHROPIC_RETRY", 503).await;
}

/// `529` is Anthropic's documented "overloaded" status and sits outside the
/// standard `5xx` codes, so an explicit list of those once let it end the
/// session on the first occurrence (#332). Not `start_paused`, as above.
#[tokio::test]
async fn overloaded_529_is_retried_at_the_http_layer() {
    assert_retried_at_the_http_layer("OUTRIG_TEST_ANTHROPIC_RETRY_529", 529).await;
}

/// One model call answered `status` and then a reply: the turn completes, and
/// the retry sent the same request again rather than rebuilding the turn.
async fn assert_retried_at_the_http_layer(var: &str, status: u16) {
    let (addr, mut requests) = start_mock_http(vec![
        CannedResponse::status(
            status,
            json!({
                "type": "error",
                "error": { "type": "overloaded_error", "message": "overloaded" },
            }),
        ),
        text_reply("Recovered."),
    ])
    .await;

    let reply = run_one_turn(addr, var, MODEL, Some(1024), vec![])
        .await
        .expect("the retry should carry the turn through the failed call");

    assert_eq!(reply, "Recovered.");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        recorded.len(),
        2,
        "the failed call is retried once: {recorded:#?}",
    );
    assert_eq!(
        recorded[0].body, recorded[1].body,
        "a retry replays the same model call, not a rebuilt turn",
    );
}

/// A `Retry-After` header sets the wait, in place of the backoff curve. This
/// is the whole reason the retry moved below rig: `rig::http_client::Error`
/// keeps a status and a body and drops every header, so nothing above this
/// layer can see what the server asked for.
///
/// The assertion is the elapsed time, because that is the only observable
/// difference. Backoff at attempt 0 is jittered into `[0.5s, 1.0s]`, so a wait
/// past 1.8s cannot have come from the curve.
#[tokio::test]
async fn retry_after_header_is_honored() {
    let (addr, mut requests) = start_mock_http(vec![
        CannedResponse::status(
            429,
            json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": "slow down" },
            }),
        )
        .with_header("Retry-After", "2"),
        text_reply("Recovered."),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_RETRY_AFTER";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(30));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let started = std::time::Instant::now();
    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("the retry should carry the turn through the 429")
        .reply;
    let elapsed = started.elapsed();

    assert_eq!(reply, "Recovered.");
    assert!(
        elapsed >= std::time::Duration::from_millis(1800),
        "waited {elapsed:?}, which is the backoff curve rather than the header",
    );

    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        recorded.len(),
        2,
        "the rate-limited call is retried once: {recorded:#?}",
    );
    assert_eq!(
        recorded[0].body, recorded[1].body,
        "a retry replays the same model call, not a rebuilt turn",
    );
}

/// An endpoint that stays broken through the whole retry budget ends the
/// *turn*, not the process -- and the same agent takes the next turn.
///
/// Before this, the `CompletionError` reached `repl.rs`'s `res?`, unwound past
/// the REPL loop, tore the containers down, and exited 1 with the conversation
/// lost. `retry-budget-secs = 0` makes the giving-up immediate, so the test
/// pins the recovery rather than the waiting.
#[tokio::test]
async fn exhausted_budget_ends_the_turn_without_killing_the_agent() {
    let (addr, mut requests) = start_mock_http(vec![
        CannedResponse::status(
            429,
            json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": "slow down" },
            }),
        ),
        text_reply("Second turn."),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_BUDGET_SPENT";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("a spent budget ends the turn, it does not fail the session")
        .reply;
    assert_eq!(
        reply, "",
        "the model never spoke, so nothing belongs on stdout",
    );
    assert!(
        history.is_empty(),
        "nothing was appended, which is what the advice to resend rests on: {history:#?}",
    );
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "a zero budget makes the first failure final",
    );

    // The agent is still usable, which is the point.
    let reply = agent
        .run_turn("try again", &mut history)
        .await
        .expect("the next turn runs on the same agent")
        .reply;
    assert_eq!(reply, "Second turn.");
    assert_eq!(drain_recorded(&mut requests).len(), 1);
}

/// A `200 OK` carrying no usable content is retried at the model layer, which
/// is the only layer that can see it -- to the HTTP client it is a success.
///
/// `stop_reason` is deliberately not `end_turn`: rig normalizes *that* empty
/// response into empty assistant text, and every other one into the
/// `ResponseError` this retries.
///
/// Not `start_paused`, for the reason
/// [`transient_status_is_retried_at_the_http_layer`] gives: the real wait here
/// is one jittered base delay, at most a second.
#[tokio::test]
async fn an_unusable_response_is_retried_at_the_model_layer() {
    let (addr, mut requests) = start_mock_http(vec![
        message(json!([]), "max_tokens"),
        text_reply("Recovered."),
    ])
    .await;

    let reply = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_UNUSABLE",
        MODEL,
        Some(1024),
        vec![],
    )
    .await
    .expect("the retry should carry the turn through the unusable response");

    assert_eq!(reply, "Recovered.");

    let recorded = drain_recorded(&mut requests);
    assert_eq!(
        recorded.len(),
        2,
        "the unusable response is retried once: {recorded:#?}",
    );
    assert_eq!(
        recorded[0].body, recorded[1].body,
        "a retry replays the same model call, not a rebuilt turn",
    );
}

/// The property the whole design rests on: a retry replays one *model call*,
/// not the turn around it. The unusable response lands on the second model
/// call, after a tool has already run, and the tool does not run again.
///
/// Retrying `agent.prompt(..)` instead would re-execute the tool -- harmless
/// for this echo, a repeat for a container tool call that wrote something.
#[tokio::test]
async fn a_retry_mid_turn_does_not_re_run_the_tool_calls_before_it() {
    let (addr, mut requests) = start_mock_http(vec![
        message(
            json!([{
                "type": "tool_use",
                "id": "toolu_mock_1",
                "name": "outrig_test_echo",
                "input": { "value": "ping" }
            }]),
            "tool_use",
        ),
        message(json!([]), "max_tokens"),
        text_reply("The echo said pong:ping."),
    ])
    .await;

    let tool = EchoTool::default();
    let reply = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_UNUSABLE_MID_TURN",
        MODEL,
        Some(4096),
        vec![tool.clone()],
    )
    .await
    .expect("the retry should carry the turn through the unusable response");

    assert_eq!(reply, "The echo said pong:ping.");
    assert_eq!(
        tool.call_count(),
        1,
        "the retry replays the model call, not the turn, so the tool runs once",
    );

    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 3, "tool_use, the retry, the reply");
    assert_eq!(
        recorded[1].body, recorded[2].body,
        "the retried call carries the same history the unusable one did -- \
         including the tool_result, which is what was at risk",
    );
}

/// The same property one layer down, and the one that decides where failover
/// sits: a move replays one *model call* against the next candidate, not the
/// turn around it. The head serves a `tool_use`, the tool runs, and only then
/// does the head go down -- so the move happens with a tool result already in
/// the history, which is the state re-running the turn would destroy.
///
/// Failing over around `agent.prompt(..)` instead would re-execute the tool --
/// harmless for this echo, a repeat for a container tool call that wrote
/// something. `llm/failover.rs`'s module doc cites this test by name for that
/// argument, and 0002-36 asked for it as a test rather than a comment.
///
/// It carries the head's `503` case too: the first candidate fails past its
/// bounds -- zero, here -- and the turn completes on the second.
#[tokio::test]
async fn tools_run_before_a_move_are_not_re_executed() {
    let (head, mut head_requests) = start_mock_http(vec![
        message(
            json!([{
                "type": "tool_use",
                "id": "toolu_mock_1",
                "name": "outrig_test_echo",
                "input": { "value": "ping" }
            }]),
            "tool_use",
        ),
        // Down from here on: `start_mock_http` repeats its last entry, so any
        // further request to the head is a `503` as well.
        CannedResponse::status(503, json!({ "type": "error" })),
    ])
    .await;
    let (next, mut next_requests) =
        start_mock_http(vec![text_reply("The echo said pong:ping.")]).await;

    let vars = [
        "OUTRIG_TEST_ANTHROPIC_FAILOVER_HEAD",
        "OUTRIG_TEST_ANTHROPIC_FAILOVER_NEXT",
    ];
    let cfg = mock_chain_config(head, next, vars);
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &vars, vec![tool.clone()]).await;

    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("the move should carry the turn through the head's outage")
        .reply;

    assert_eq!(
        reply, "The echo said pong:ping.",
        "the turn finished on the second candidate",
    );
    assert_eq!(
        tool.call_count(),
        1,
        "the move replays the model call, not the turn, so the tool runs once",
    );

    let head_recorded = drain_recorded(&mut head_requests);
    let next_recorded = drain_recorded(&mut next_requests);
    assert_eq!(head_recorded.len(), 2, "the tool_use, then the 503");
    assert_eq!(
        next_recorded.len(),
        1,
        "one model call moved, not the turn around it: {next_recorded:#?}",
    );
    assert_eq!(
        head_recorded[1].body, next_recorded[0].body,
        "the moved call carries the same history the failed one did -- \
         including the tool_result, which is what was at risk",
    );
    assert!(
        next_recorded[0].body.to_string().contains("tool_result"),
        "the move landed mid-turn, after a tool had already run: {:#?}",
        next_recorded[0].body,
    );
}

/// A response that stays unusable ends the *turn*, not the process -- and the
/// same agent takes the next turn.
///
/// This is the failure that prompted the change: an `outrig run` six turns into
/// a conversation exited 1 with `agent prompt failed: CompletionError:
/// ResponseError: Response contained no message or tool call (empty)`, tearing
/// down its containers and losing the lot. `retry-budget-secs = 0` makes the
/// giving-up immediate, so the test pins the recovery rather than the waiting.
#[tokio::test]
async fn a_persistently_unusable_response_ends_the_turn_not_the_session() {
    let (addr, mut requests) = start_mock_http(vec![
        message(json!([]), "max_tokens"),
        text_reply("Second turn."),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_UNUSABLE_PERSISTS";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("an unusable response ends the turn, it does not fail the session")
        .reply;
    assert_eq!(
        reply, "",
        "the model never said anything usable, so nothing belongs on stdout",
    );
    assert!(
        history.is_empty(),
        "nothing was appended, which is what the advice to resend rests on: {history:#?}",
    );
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "a zero budget makes the first unusable response final",
    );

    // The agent is still usable, which is the point.
    let reply = agent
        .run_turn("try again", &mut history)
        .await
        .expect("the next turn runs on the same agent")
        .reply;
    assert_eq!(reply, "Second turn.");
    assert_eq!(drain_recorded(&mut requests).len(), 1);
}

/// The other side of the two recoverable classes: a rejected key still ends
/// the session. Both of the arms above return `Ok` for a failing model call, so
/// this pins that the *terminal* class did not get swept in with them -- a
/// misconfiguration the user must fix has to exit, not invite a resend that can
/// never work.
#[tokio::test]
async fn a_rejected_api_key_stays_fatal() {
    let (addr, mut requests) = start_mock_http(vec![CannedResponse::status(
        401,
        json!({
            "type": "error",
            "error": { "type": "authentication_error", "message": "invalid x-api-key" },
        }),
    )])
    .await;

    let err = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_BAD_KEY",
        MODEL,
        Some(1024),
        vec![],
    )
    .await
    .expect_err("a rejected key is the user's to fix, so it must not end up an Ok turn");

    assert!(
        err.to_string().contains("401"),
        "the error should name the status: {err}",
    );
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "a 401 is terminal at both retry layers",
    );
}

/// A redirect is terminal: outrig follows none, even to the provider's own
/// address, and a replay would be answered the same. So it is sent once and
/// stays fatal, as a `401` does, rather than ending only the turn. The budget
/// is the shipped default, so a redirect that is retried runs into the
/// timeout.
#[tokio::test]
async fn a_redirect_is_fatal_at_once() {
    let (addr, mut requests) = start_mock_http(vec![
        CannedResponse::status(307, json!({})).with_header("Location", "/v1/messages"),
    ])
    .await;

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_one_turn(
            addr,
            "OUTRIG_TEST_ANTHROPIC_REDIRECT_LOOP",
            MODEL,
            Some(1024),
            vec![],
        ),
    )
    .await
    .expect("a redirect must not be retried for the whole budget")
    .expect_err("no resend can satisfy a redirect, so it must not end up an Ok turn");

    assert!(
        err.to_string().contains("307"),
        "the error should name the status: {err}",
    );
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "the redirect is neither followed nor retried",
    );
}

/// The model asking for one call to `name`, under the id the assertions below
/// look for.
fn tool_use(name: &str, input: Value) -> CannedResponse {
    message(
        json!([{
            "type": "tool_use",
            "id": "toolu_mock_1",
            "name": name,
            "input": input
        }]),
        "tool_use",
    )
}

/// The model asking for one echo.
fn echo_tool_use() -> CannedResponse {
    tool_use("outrig_test_echo", json!({ "value": "ping" }))
}

fn rate_limited() -> CannedResponse {
    CannedResponse::status(
        429,
        json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": "slow down" },
        }),
    )
}

/// Assert that `history` is `prior` followed by the turn that asked for one
/// echo and ran it: the prompt, the assistant's `tool_use`, then the user turn
/// carrying its `tool_result` -- the order Anthropic requires -- and nothing
/// after it.
fn assert_kept_the_echo(history: &[Message], prior: &[Message], prompt: &str) {
    assert!(
        history.starts_with(prior),
        "the conversation before the turn is untouched: {history:#?}",
    );
    let [asked, called, answered] = &history[prior.len()..] else {
        panic!("the turn keeps its prompt, the tool call, and its result: {history:#?}");
    };
    assert_eq!(asked, &Message::user(prompt));
    let Message::Assistant(AssistantMessage { content, .. }) = called else {
        panic!("the tool call is the assistant's: {called:#?}");
    };
    assert!(
        content.iter().any(|part| matches!(
            part,
            AssistantContent::ToolCall(call)
                if call.id.wire() == "toolu_mock_1"
                    && call.function.name.as_str() == "outrig_test_echo"
        )),
        "the assistant turn carries the tool_use that ran: {called:#?}",
    );
    let Message::User { content } = answered else {
        panic!("the result comes back on a user turn: {answered:#?}");
    };
    assert!(
        content.iter().any(|part| matches!(
            part,
            UserContent::ToolResult(result)
                if result.call.wire() == "toolu_mock_1"
                    && result.content.iter().any(|item| matches!(
                        item,
                        ToolResultContent::Text(text) if text.text.contains("pong:ping")
                    ))
        )),
        "the result answers that call with the tool's output: {answered:#?}",
    );
}

/// Assert that a request carries the echo's `tool_use`, answered by its
/// `tool_result` in the very next message, and ends with `prompt` -- which is
/// how the next turn learns the tool already ran.
fn assert_request_carries_the_echo(request: &RecordedRequest, prompt: &str) {
    let messages = request.body["messages"].as_array().expect("messages array");
    let called = messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block["type"] == "tool_use" && block["id"] == "toolu_mock_1")
                })
        })
        .unwrap_or_else(|| panic!("the request lacks the tool_use that ran: {messages:#?}"));
    let answered = messages
        .get(called + 1)
        .unwrap_or_else(|| panic!("nothing answers the tool_use: {messages:#?}"));
    assert_eq!(answered["role"], "user");
    assert_eq!(answered["content"][0]["type"], "tool_result");
    assert_eq!(
        answered["content"][0]["tool_use_id"], "toolu_mock_1",
        "the result must name the call it answers",
    );
    assert!(
        answered.to_string().contains("pong:ping"),
        "the tool's output reaches the model: {answered:#?}",
    );
    let last = messages.last().expect("a prompt");
    assert!(
        last["role"] == "user" && last.to_string().contains(prompt),
        "the new prompt follows the kept work: {messages:#?}",
    );
}

/// Drive a turn until the mock has recorded `calls` requests -- the last held
/// open -- then drop it inside that model call. That is what `repl.rs` does on
/// Ctrl-C: it races the turn against the interrupt, and the losing future is
/// dropped mid-await.
async fn drop_turn_in_call(
    agent: &RigAgent,
    prompt: &str,
    history: &mut Vec<Message>,
    requests: &mut tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
    calls: usize,
) {
    let turn = std::pin::pin!(agent.run_turn(prompt, history));
    let parked = async {
        for _ in 0..calls {
            requests.recv().await.expect("the mock is serving");
        }
    };
    tokio::select! {
        end = turn => panic!("a held call keeps the turn waiting, got: {end:?}"),
        () = parked => {}
    }
}

/// The case #197 is about: a turn runs a tool, and its *next* model call
/// fails for good. Retries are off, so the `429` is final at once.
///
/// This used to leave the history as it stood before the prompt and advise
/// sending the prompt again, which would run the tool again -- harmless for
/// this echo, a repeat for a container tool call that wrote something. The
/// tool call and its result are kept instead, and the next turn's request
/// carries them, so a `continue` picks up from the work.
#[tokio::test]
async fn a_failed_call_after_a_tool_call_keeps_it() {
    let (addr, mut requests) = start_mock_http(vec![
        echo_tool_use(),
        rate_limited(),
        text_reply("Picked up from the echo."),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_FAILED_AFTER_TOOL";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &[var], vec![tool.clone()]).await;

    let prior = vec![
        Message::user("what is this repo?"),
        Message::assistant("outrig"),
    ];
    let mut history = prior.clone();
    let end = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("a failed call ends the turn, it does not fail the session");

    assert!(
        matches!(end.stopped, Some(TurnStop::EndpointFailed(_))),
        "got: {:?}",
        end.stopped,
    );
    assert_eq!(end.reply, "", "the model spoke only in the tool call");
    assert_eq!(tool.call_count(), 1);
    assert_kept_the_echo(&history, &prior, "echo ping for me");
    assert_eq!(
        drain_recorded(&mut requests).len(),
        2,
        "the tool_use, then the 429, final with retries off",
    );

    let reply = agent
        .run_turn("continue", &mut history)
        .await
        .expect("the next turn runs on the same agent")
        .reply;
    assert_eq!(reply, "Picked up from the echo.");
    assert_eq!(
        tool.call_count(),
        1,
        "continuing does not run the tool again"
    );
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    assert_request_carries_the_echo(&recorded[0], "continue");
}

/// A response that stays unusable ends the turn the same way, and keeps the
/// same work.
#[tokio::test]
async fn an_unusable_response_after_a_tool_call_keeps_it() {
    let (addr, _requests) =
        start_mock_http(vec![echo_tool_use(), message(json!([]), "max_tokens")]).await;

    let var = "OUTRIG_TEST_ANTHROPIC_UNUSABLE_AFTER_TOOL";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &[var], vec![tool.clone()]).await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("an unusable response ends the turn, it does not fail the session");

    assert!(
        matches!(end.stopped, Some(TurnStop::EndpointFailed(_))),
        "got: {:?}",
        end.stopped,
    );
    assert_eq!(tool.call_count(), 1);
    assert_kept_the_echo(&history, &[], "echo ping for me");
}

/// So does an alias chain whose every candidate failed, while one of them
/// failed for a reason a later prompt can get past.
#[tokio::test]
async fn an_exhausted_chain_after_a_tool_call_keeps_it() {
    let down = || CannedResponse::status(503, json!({ "type": "error" }));
    let (head, _head_requests) = start_mock_http(vec![echo_tool_use(), down()]).await;
    let (next, _next_requests) = start_mock_http(vec![down()]).await;

    let vars = [
        "OUTRIG_TEST_ANTHROPIC_CHAIN_AFTER_TOOL_HEAD",
        "OUTRIG_TEST_ANTHROPIC_CHAIN_AFTER_TOOL_NEXT",
    ];
    let cfg = mock_chain_config(head, next, vars);
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &vars, vec![tool.clone()]).await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("a chain that failed recoverably ends the turn, not the session");

    assert_eq!(
        end.stopped,
        Some(TurnStop::EndpointFailed(
            "every model candidate failed".to_string()
        )),
    );
    assert_eq!(tool.call_count(), 1);
    assert_kept_the_echo(&history, &[], "echo ping for me");
}

/// A turn that fails on its *first* model call has run nothing, so the history
/// is exactly what it was, and sending the prompt again is the right advice.
/// Here that comes straight after a turn that did run a tool, whose work must
/// not come back a second time.
#[tokio::test]
async fn a_failure_on_the_first_call_leaves_the_history_as_it_was() {
    let (addr, _requests) = start_mock_http(vec![
        echo_tool_use(),
        text_reply("The echo said pong:ping."),
        rate_limited(),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_FIRST_CALL_FAILS";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &[var], vec![tool.clone()]).await;

    let mut history = Vec::new();
    agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("the first turn completes");
    let before = history.clone();

    let end = agent
        .run_turn("and once more", &mut history)
        .await
        .expect("a failed call ends the turn, it does not fail the session");

    assert!(
        matches!(end.stopped, Some(TurnStop::EndpointFailed(_))),
        "got: {:?}",
        end.stopped,
    );
    assert_eq!(
        history, before,
        "nothing ran, so nothing is added -- not even the prompt",
    );
    assert_eq!(tool.call_count(), 1);
}

/// Ctrl-C drops the turn's future mid-await, and no error comes back to splice
/// from. Dropped inside its second model call, after its tool ran, the turn
/// keeps the tool call and its result as a failed call does, and the next turn
/// carries them.
#[tokio::test]
async fn a_turn_dropped_mid_call_keeps_the_tool_calls_before_it() {
    let (addr, mut requests) = start_mock_http(vec![
        echo_tool_use(),
        CannedResponse::held(),
        text_reply("Picked up from the echo."),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_DROPPED_AFTER_TOOL";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let tool = EchoTool::default();
    let agent = build_mock_agent(&cfg, &[var], vec![tool.clone()]).await;

    let prior = vec![
        Message::user("what is this repo?"),
        Message::assistant("outrig"),
    ];
    let mut history = prior.clone();
    drop_turn_in_call(&agent, "echo ping for me", &mut history, &mut requests, 2).await;

    assert_eq!(tool.call_count(), 1);
    assert_kept_the_echo(&history, &prior, "echo ping for me");

    let reply = agent
        .run_turn("continue", &mut history)
        .await
        .expect("the next turn runs on the same agent")
        .reply;
    assert_eq!(reply, "Picked up from the echo.");
    assert_eq!(
        tool.call_count(),
        1,
        "continuing does not run the tool again"
    );
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    assert_request_carries_the_echo(&recorded[0], "continue");
}

/// Dropped inside its first model call, a turn has run nothing, and the
/// history is left as it was.
#[tokio::test]
async fn a_turn_dropped_in_its_first_call_adds_nothing() {
    let (addr, mut requests) = start_mock_http(vec![CannedResponse::held()]).await;

    let var = "OUTRIG_TEST_ANTHROPIC_DROPPED_FIRST_CALL";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![EchoTool::default()]).await;

    let prior = vec![
        Message::user("what is this repo?"),
        Message::assistant("outrig"),
    ];
    let mut history = prior.clone();
    drop_turn_in_call(&agent, "echo ping for me", &mut history, &mut requests, 1).await;

    assert_eq!(history, prior);
}

/// Anthropic requires `max_tokens` on every request, and rig only knows a
/// default for the identifiers it recognizes. This pins all three tiers of the
/// precedence OutRig applies -- config, rig's published ceiling, OutRig's
/// fallback -- including, in both directions, that the fallback fills only the
/// gap and never overrides a ceiling rig or the user already chose.
#[tokio::test]
async fn max_tokens_comes_from_config_then_rig_then_the_fallback() {
    // 1. Recognized identifier, nothing configured: rig's catalog ceiling for
    //    the identifier travels on the request.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_KNOWN", MODEL, None).await,
        MODEL_CEILING,
        "rig's published ceiling for the identifier, not OutRig's fallback",
    );

    // 2. Another recognized identifier, whose published ceiling differs from
    //    the first's and is higher than the fallback. Together with case 1
    //    this is what would catch a fallback that stopped checking whether rig
    //    already had an answer: it would show up here as a ceiling *lowered*
    //    to 32768.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_OTHER", OTHER_CEILING_MODEL, None).await,
        OTHER_CEILING,
    );

    // 3. Unrecognized identifier, nothing configured: rig has no ceiling to
    //    offer, so OutRig's fallback fills the gap and the turn runs -- see
    //    `rig_publishes_a_ceiling_only_for_the_identifiers_it_knows` below.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_UNKNOWN", UNRECOGNIZED_MODEL, None).await,
        outrig_cli::llm::ANTHROPIC_FALLBACK_MAX_TOKENS,
    );

    // 4. Unrecognized identifier with a configured ceiling: that value is
    //    what travels, over the fallback.
    assert_eq!(
        recorded_max_tokens(
            "OUTRIG_TEST_ANTHROPIC_CONFIGURED",
            UNRECOGNIZED_MODEL,
            Some(8192),
        )
        .await,
        8192,
    );
}

/// A configured ceiling above what the identifier can actually serve is lowered
/// to the published one rather than sent and refused.
///
/// The Messages API rejects an over-ceiling `max_tokens` outright, so the whole
/// turn fails rather than being cut short -- lowering is the only outcome that
/// runs at all. This is distinct from the fallback in
/// `max_tokens_comes_from_config_then_rig_then_the_fallback`, whose cases 1 and 2
/// pin that a ceiling is never lowered to OutRig's 32768: here the value asserted
/// is the identifier's own published ceiling, so a fallback that started
/// overriding rig would fail this test with 32768 rather than pass it.
#[tokio::test]
async fn a_configured_ceiling_is_capped_at_the_published_one() {
    // Recognized identifier, configured above its published ceiling. The
    // number that travels is the model's, not the config's.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_OVER_CEILING", MODEL, Some(256_000)).await,
        MODEL_CEILING,
        "the identifier's published ceiling, not the configured 256000 the API \
         would refuse -- and not the fallback",
    );

    // Under the ceiling, nothing to cap: the configured value is what the user
    // asked for and travels untouched.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_UNDER_CEILING", MODEL, Some(16_384)).await,
        16_384,
    );

    // No published ceiling to cap against: OutRig invents none, so a large
    // configured value travels whole. Capping here would mean guessing, and the
    // guess would be wrong for exactly the identifiers rig has not caught up to.
    // Deliberately larger than the fallback, unlike case 4 of the test above: a
    // cap mistakenly applied against 32768 rather than rig's ceiling passes
    // there and fails here.
    assert_eq!(
        recorded_max_tokens(
            "OUTRIG_TEST_ANTHROPIC_UNCAPPED",
            UNRECOGNIZED_MODEL,
            Some(256_000),
        )
        .await,
        256_000,
    );
}

/// The catalog contract this integration depends on, asserted directly
/// against rig so an upgrade that changes a default fails here rather than
/// in a truncated reply months later.
///
/// rig's Anthropic wire leaves the default unset for an identifier its
/// catalog does not list, and that `None` is the signal the fallback ceiling
/// keys off.
#[test]
fn rig_publishes_a_ceiling_only_for_the_identifiers_it_knows() {
    use rig::providers::anthropic::wire::AnthropicConfig;

    let config = AnthropicConfig::new("unused-no-request-is-made");
    let published = |identifier: &str| config.completion(identifier).default_max_tokens;

    assert_eq!(published("claude-sonnet-4-6"), Some(128_000));
    assert_eq!(published("claude-opus-4-6"), Some(128_000));
    assert_eq!(
        published("claude-3-5-sonnet-20241022"),
        None,
        "an unrecognized identifier must arrive with no ceiling, so build_agent \
         can tell that gap from a ceiling rig chose",
    );
}

/// A turn whose only content is a thinking block is reported, not swallowed.
///
/// A think-heavy turn the model ends on its own comes back carrying reasoning
/// and no text. Rig treats that as an ordinary success whose `output` is the
/// empty string -- text parts are all `output` concatenates -- so before this,
/// `run_turn` returned `""` with no stop reason, the REPL's
/// `if !reply.is_empty()` guard printed nothing, and a minute of billed
/// generation reached the user as silence indistinguishable from outrig having
/// ignored the prompt.
///
/// Pins all three of the properties that make it reportable: the turn is
/// recognizably silent, the reasoning is recovered rather than dropped, and the
/// report says which of the two silences this was.
///
/// Then pins what the next prompt sends: the turn, as the signed `thinking`
/// block it came as.
#[tokio::test]
async fn a_reasoning_only_turn_is_recovered_and_reported() {
    let thinking = json!({
        "type": "thinking",
        "thinking": "weighing the two-image split against one",
        "signature": "sig-1",
    });
    let (addr, mut requests) = start_mock_http(vec![
        message(json!([thinking.clone()]), "end_turn"),
        text_reply("ok"),
    ])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_REASONING_ONLY";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("two images, then", &mut history)
        .await
        .expect("a reasoning-only turn ends the turn, it does not fail the session");

    assert_eq!(
        end.reply, "",
        "rig's `output` is text-parts-only, so the reply is genuinely empty",
    );
    assert!(
        end.stopped.is_none(),
        "nothing cut this turn short -- the model finished on its own, which is \
         exactly why nothing else would have reported it: {:?}",
        end.stopped,
    );
    assert!(
        end.is_silent(),
        "a turn with no text and no stop reason is the one outcome that used to \
         reach the user as pure silence",
    );
    assert_eq!(
        end.recovered.as_deref(),
        Some("weighing the two-image split against one"),
        "the reasoning the user paid for is salvaged instead of dropped",
    );
    let report = end.silent_report();
    assert!(
        report.contains("weighing the two-image split against one"),
        "the report carries the recovered reasoning: {report}",
    );
    assert!(
        report.contains("hidden reasoning"),
        "the report names which silence this was, so the user can act on it: {report}",
    );

    assert!(
        !history.is_empty(),
        "the turn was completed, not abandoned, so its history is retained and the \
         advice to send \"continue\" is honest: {history:#?}",
    );
    assert_eq!(
        drain_recorded(&mut requests).len(),
        1,
        "a reasoning-only turn is a completed turn, not a retryable one",
    );

    agent
        .run_turn("continue", &mut history)
        .await
        .expect("the next prompt is answered");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1, "one request for the next turn");
    let messages = recorded[0].messages();
    assert_eq!(
        recorded[0].roles(),
        ["user", "assistant", "user"],
        "{messages:#?}",
    );
    assert_eq!(
        messages[1],
        json!({ "role": "assistant", "content": [thinking] }),
        "the thinking goes back exactly as Anthropic issued it",
    );
}

/// A turn of only reasoning that was cut off at the output ceiling is a turn
/// rig fails: reasoning is not an answer, and a turn cut short of one is not
/// a turn the model finished. It ends the turn as the endpoint's failure,
/// naming rig's reason, and leaves the history as it was.
///
/// What a user used to be shown of such a turn was its recovered reasoning
/// ([`a_reasoning_only_turn_is_recovered_and_reported`], which now takes a turn
/// the model ended on its own); what they are shown now is why nothing came
/// back and the ceiling to raise.
#[tokio::test]
async fn a_reasoning_only_turn_cut_off_at_the_ceiling_ends_the_turn() {
    let (addr, _requests) = start_mock_http(vec![message(
        json!([{ "type": "thinking", "thinking": "weighing it", "signature": "sig-1" }]),
        "max_tokens",
    )])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_REASONING_CUT_OFF";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("two images, then", &mut history)
        .await
        .expect("a cut-off turn ends the turn, it does not fail the session");

    let Some(TurnStop::EndpointFailed(reason)) = &end.stopped else {
        panic!("a turn cut short of an answer is the endpoint's failure: {end:?}");
    };
    assert!(
        reason.contains("no answer") && reason.contains("max_tokens"),
        "the reason names what rig found and the ceiling to raise: {reason}",
    );
    assert!(history.is_empty(), "nothing is kept of it: {history:#?}");
}

/// A whitespace-only text part alongside the reasoning is still a silent turn,
/// and its reasoning is still salvaged.
///
/// The two questions -- "is this turn silent?" and "is there non-text content
/// worth recovering?" -- are the same question, and they were once asked with
/// different predicates: `trim`-based for silence, exact-emptiness for
/// recovery. A turn shaped like this one fell in the gap, reporting "without
/// producing any content at all" while the reasoning it did produce went
/// unread. Nothing trims text parts on the way in, so the shape reaches
/// `TurnEnd` exactly as the provider sent it.
#[tokio::test]
async fn a_whitespace_reply_beside_reasoning_is_still_recovered() {
    let (addr, _requests) = start_mock_http(vec![message(
        json!([
            { "type": "thinking", "thinking": "still weighing it", "signature": "sig-1" },
            { "type": "text", "text": "   " },
        ]),
        "end_turn",
    )])
    .await;

    let var = "OUTRIG_TEST_ANTHROPIC_WHITESPACE_REPLY";
    let cfg = mock_config(addr, var, MODEL, Some(1024), Some(0));
    let agent = build_mock_agent(&cfg, &[var], vec![]).await;

    let mut history = Vec::new();
    let end = agent
        .run_turn("two images, then", &mut history)
        .await
        .expect("a whitespace reply ends the turn, it does not fail the session");

    assert!(
        end.is_silent(),
        "whitespace renders as a blank line, which is not a reply: {:?}",
        end.reply,
    );
    assert_eq!(
        end.recovered.as_deref(),
        Some("still weighing it"),
        "the recovery gate must read blankness the same way the silence gate does",
    );
    let report = end.silent_report();
    assert!(
        report.contains("hidden reasoning"),
        "the cause must be the one that happened, not \"no content at all\": {report}",
    );
    assert!(
        report.contains("still weighing it"),
        "and the reasoning must reach the user: {report}",
    );
}

/// Reasoning Anthropic did not issue is not sent back to it as thinking.
///
/// An alias chain shares one history across its candidates, so a turn that a
/// local fallback answered is still in it when the next call starts back at the
/// Anthropic head. An OpenAI-style endpoint's `reasoning_content` comes back
/// with no signature, since only Anthropic signs, and Anthropic refuses a
/// `thinking` block without one. So the head would fail every call, long after
/// its outage ended, and the chain would stay on the fallback without saying
/// why. rig sends such reasoning as plain text instead, which Anthropic takes,
/// and replays Anthropic's own thinking only from a turn it knows to be
/// Anthropic's.
///
/// The history below is what such a fallback leaves behind, next to a turn
/// Anthropic answered itself. What this pins is the request the head is sent.
#[tokio::test]
async fn reasoning_anthropic_did_not_issue_is_not_sent_back_to_it() {
    let (head, mut head_requests) = start_mock_http(vec![text_reply("ok")]).await;
    let (next, mut next_requests) = start_mock_http(vec![text_reply("unused")]).await;
    let vars = [
        "OUTRIG_TEST_ANTHROPIC_FOREIGN_REASONING_HEAD",
        "OUTRIG_TEST_ANTHROPIC_FOREIGN_REASONING_NEXT",
    ];
    let cfg = mock_chain_config(head, next, vars);
    let agent = build_mock_agent(&cfg, &vars, vec![]).await;

    let assistant = |parts: Vec<AssistantContent>| Message::Assistant(AssistantMessage::new(parts));
    let mut history = vec![
        Message::user("first"),
        // Anthropic's own turn, as rig keeps one: its origin names the model
        // that wrote it, and the signature rides in the block's provider item.
        Message::Assistant(
            AssistantMessage::new(vec![
                AssistantContent::Reasoning(Reasoning::new("claude weighs it")).with_native(
                    json!({
                        "type": "thinking",
                        "thinking": "claude weighs it",
                        "signature": "sig-1",
                    }),
                ),
                AssistantContent::Reasoning(Reasoning {
                    redacted: true,
                    ..Reasoning::new("")
                })
                .with_native(json!({ "type": "redacted_thinking", "data": "opaque-blob" })),
                AssistantContent::text("claude's answer"),
            ])
            .with_origin(Origin::new("anthropic.messages", "anthropic", MODEL)),
        ),
        Message::user("second"),
        // A local fallback's turn: unsigned reasoning beside its reply...
        assistant(vec![
            AssistantContent::reasoning("local weighs it"),
            AssistantContent::text("local answer"),
        ]),
        Message::user("third"),
        // ...and one that was nothing but reasoning.
        assistant(vec![AssistantContent::reasoning("local only thought")]),
    ];

    agent
        .run_turn("fourth", &mut history)
        .await
        .expect("the head takes the turn");

    let recorded = drain_recorded(&mut head_requests);
    assert_eq!(recorded.len(), 1, "the head answered, first time");
    assert!(
        drain_recorded(&mut next_requests).is_empty(),
        "and the chain never moved",
    );

    let messages = recorded[0].body["messages"]
        .as_array()
        .expect("a messages array");
    let blocks: Vec<&Value> = messages
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .collect();
    assert!(
        blocks
            .iter()
            .filter(|b| b["type"] == "thinking")
            .all(|b| b["signature"].is_string()),
        "Anthropic refuses a thinking block without its signature: {blocks:#?}",
    );
    assert!(
        blocks.iter().any(|b| b["type"] == "thinking"
            && b["thinking"] == "claude weighs it"
            && b["signature"] == "sig-1")
            && blocks
                .iter()
                .any(|b| b["type"] == "redacted_thinking" && b["data"] == "opaque-blob"),
        "Anthropic's own thinking goes back as it came: {blocks:#?}",
    );
    assert!(
        blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "local weighs it"),
        "the fallback's reasoning goes back as text, which Anthropic takes: {blocks:#?}",
    );
    assert_eq!(
        messages.iter().filter(|m| m["role"] == "assistant").count(),
        3,
        "the reasoning-only turn goes back as a text turn, not left out or sent \
         empty, which Anthropic refuses: {messages:#?}",
    );
}
