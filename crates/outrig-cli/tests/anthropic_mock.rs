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
//! * which of the three `max_tokens` tiers -- config, rig's published ceiling,
//!   OutRig's fallback -- reaches the wire for a model identifier rig
//!   recognizes and one it does not, including the difference between
//!   `completion_model` (what `build_agent` uses) and
//!   `CompletionModel::with_model` (which would silently cap replies at 2048);
//! * the shared retry client covers this provider too -- including that a
//!   `Retry-After` sets the wait, and that a spent budget ends the turn
//!   without taking the session with it.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::{Value, json};

use outrig::config::Config;
use outrig_cli::llm::{build_agent, resolve_agent};
use outrig_cli::session_tool;

use common::{CannedResponse, drain_recorded, set_test_env, start_mock_http, unset_test_env};

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

impl ToolDyn for EchoTool {
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

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let parsed: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
            let value = parsed
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(format!("pong:{value}"))
        })
    }
}

// ---- canned Anthropic responses -------------------------------------------

const KEY: &str = "sk-ant-mock-key";
const PREAMBLE: &str = "You are a careful coding assistant.";
/// Low enough that a test can drive the agent into the cap in one turn.
const TOOL_CALL_MAX: usize = 1;
const MODEL: &str = "claude-sonnet-4-6";
/// A recognized identifier whose published ceiling differs from both `MODEL`'s
/// and outrig's fallback, so a test can tell the three apart by the number
/// alone.
const HIGHER_CEILING_MODEL: &str = "claude-opus-4-7";
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
    let agent = build_mock_agent(&cfg, var, tools).await;
    let mut history = Vec::new();
    agent
        .run_turn("echo ping for me", &mut history)
        .await
        .map_err(anyhow::Error::from)
}

/// Build an agent from `cfg` through the real resolve -> build path. Split out
/// of [`run_one_turn`] so a test can drive more than one turn through the same
/// agent, which is what "the session survived" means.
async fn build_mock_agent(
    cfg: &Config,
    var: &str,
    tools: Vec<EchoTool>,
) -> outrig_cli::llm::RigAgent {
    set_test_env(var, KEY);
    let resolved = resolve_agent(cfg, "coding").expect("resolves");
    unset_test_env(var);

    #[cfg(feature = "local-llm")]
    let registry = outrig_cli::llm::LlmRegistry::new();
    build_agent(
        &resolved,
        session_tool::erase(tools),
        Path::new("."),
        #[cfg(feature = "local-llm")]
        &registry,
    )
    .await
    .expect("agent builds")
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
        json!([{ "type": "text", "text": PREAMBLE }]),
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
    let (addr, mut requests) = start_mock_http(vec![
        CannedResponse::status(
            503,
            json!({
                "type": "error",
                "error": { "type": "overloaded_error", "message": "overloaded" },
            }),
        ),
        text_reply("Recovered."),
    ])
    .await;

    let reply = run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_RETRY",
        MODEL,
        Some(1024),
        vec![],
    )
    .await
    .expect("the retry should carry the turn through the 503");

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
    let agent = build_mock_agent(&cfg, var, vec![]).await;

    let started = std::time::Instant::now();
    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("the retry should carry the turn through the 429");
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
    let agent = build_mock_agent(&cfg, var, vec![]).await;

    let mut history = Vec::new();
    let reply = agent
        .run_turn("echo ping for me", &mut history)
        .await
        .expect("a spent budget ends the turn, it does not fail the session");
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
        .expect("the next turn runs on the same agent");
    assert_eq!(reply, "Second turn.");
    assert_eq!(drain_recorded(&mut requests).len(), 1);
}

/// Anthropic requires `max_tokens` on every request, and rig only knows a
/// default for the identifiers it recognizes. This pins all three tiers of the
/// precedence OutRig applies -- config, rig's published ceiling, OutRig's
/// fallback -- including, in both directions, that the fallback fills only the
/// gap and never overrides a ceiling rig or the user already chose.
#[tokio::test]
async fn max_tokens_comes_from_config_then_rig_then_the_fallback() {
    // 1. Recognized identifier, nothing configured: rig's own default for the
    //    Claude 4 family travels on the request.
    let (addr, mut requests) = start_mock_http(vec![text_reply("ok")]).await;
    run_one_turn(addr, "OUTRIG_TEST_ANTHROPIC_KNOWN", MODEL, None, vec![])
        .await
        .expect("a recognized identifier needs no configured ceiling");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].body["max_tokens"], 64_000,
        "rig's published ceiling for the Claude 4 family, not OutRig's fallback",
    );

    // 2. Another recognized identifier, whose published ceiling is higher than
    //    the fallback. Together with case 1 this is what would catch a fallback
    //    that stopped checking whether rig already had an answer: it would show
    //    up here as a ceiling *lowered* to 32768.
    let (addr, mut requests) = start_mock_http(vec![text_reply("ok")]).await;
    run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_HIGHER",
        HIGHER_CEILING_MODEL,
        None,
        vec![],
    )
    .await
    .expect("a recognized identifier needs no configured ceiling");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["max_tokens"], 128_000);

    // 3. Unrecognized identifier, nothing configured: rig has no ceiling to
    //    offer, so OutRig's fallback fills the gap and the turn runs. It is
    //    deliberately not rig's silent 2048 -- see
    //    `rig_max_tokens_defaults_differ_between_constructors` below.
    let (addr, mut requests) = start_mock_http(vec![text_reply("ok")]).await;
    run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_UNKNOWN",
        UNRECOGNIZED_MODEL,
        None,
        vec![],
    )
    .await
    .expect("the fallback ceiling covers an identifier rig does not recognize");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].body["max_tokens"],
        outrig_cli::llm::ANTHROPIC_FALLBACK_MAX_TOKENS,
    );

    // 4. Unrecognized identifier with a configured ceiling: that value is
    //    what travels, over the fallback.
    let (addr, mut requests) = start_mock_http(vec![text_reply("ok")]).await;
    run_one_turn(
        addr,
        "OUTRIG_TEST_ANTHROPIC_CONFIGURED",
        UNRECOGNIZED_MODEL,
        Some(8192),
        vec![],
    )
    .await
    .expect("a configured ceiling covers an unrecognized identifier");
    let recorded = drain_recorded(&mut requests);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body["max_tokens"], 8192);
}

/// The constructor contract this integration depends on, asserted directly
/// against rig so an upgrade that changes either default fails here rather
/// than in a truncated reply months later.
///
/// `build_agent` must keep using `CompletionClient::completion_model`: it
/// leaves the default unset for an identifier rig does not recognize, and that
/// `None` is the signal the fallback ceiling keys off. `CompletionModel::with_model`
/// invents 2048 first, which would both hide the gap and cap replies at a
/// quarter of what the fallback chooses.
#[test]
fn rig_max_tokens_defaults_differ_between_constructors() {
    use rig::client::CompletionClient;
    use rig::providers::anthropic::{Client, completion::CompletionModel};

    let client = Client::builder()
        .api_key("unused-no-request-is-made")
        .build()
        .expect("client builds");

    assert_eq!(
        client
            .completion_model("claude-sonnet-4-6")
            .default_max_tokens,
        Some(64_000),
    );
    assert_eq!(
        client
            .completion_model("claude-opus-4-6")
            .default_max_tokens,
        Some(128_000),
    );
    assert_eq!(
        client
            .completion_model("claude-3-5-sonnet-20241022")
            .default_max_tokens,
        None,
        "an unrecognized identifier must arrive with no ceiling, so build_agent \
         can tell that gap from a ceiling rig chose",
    );

    assert_eq!(
        CompletionModel::with_model(client.clone(), "claude-3-5-sonnet-20241022")
            .default_max_tokens,
        Some(2_048),
        "with_model still invents a silent ceiling; build_agent must keep \
         using completion_model",
    );
}
