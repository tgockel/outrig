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
//!   without taking the session with it;
//! * a `200` carrying no usable content is retried above the HTTP client,
//!   which is the only place it is visible at all, and ends the turn rather
//!   than the session when it persists;
//! * an alias chain moves to its next candidate *inside* a `completion()`
//!   call, so a turn that has already run a tool keeps its history and does
//!   not re-execute it -- which needs two endpoints, and so needs two mocks.

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

/// Build an agent from `cfg` through the real resolve -> build path. Split out
/// of [`run_one_turn`] so a test can drive more than one turn through the same
/// agent, which is what "the session survived" means.
///
/// `vars` is a slice rather than one name because a failover chain has a key
/// per candidate, and candidate selection drops any row whose key is unset --
/// so a chain that set only the head's var would resolve to a chain of one.
async fn build_mock_agent(
    cfg: &Config,
    vars: &[&str],
    tools: Vec<EchoTool>,
) -> outrig_cli::llm::RigAgent {
    for var in vars {
        set_test_env(var, KEY);
    }
    let resolved = resolve_agent(cfg, Some("coding")).expect("resolves");
    for var in vars {
        unset_test_env(var);
    }

    #[cfg(feature = "local-llm")]
    let registry = std::sync::Arc::new(outrig_cli::llm::LlmRegistry::new());
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
/// argument, and 0113 asked for it as a test rather than a comment.
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

/// Anthropic requires `max_tokens` on every request, and rig only knows a
/// default for the identifiers it recognizes. This pins all three tiers of the
/// precedence OutRig applies -- config, rig's published ceiling, OutRig's
/// fallback -- including, in both directions, that the fallback fills only the
/// gap and never overrides a ceiling rig or the user already chose.
#[tokio::test]
async fn max_tokens_comes_from_config_then_rig_then_the_fallback() {
    // 1. Recognized identifier, nothing configured: rig's own default for the
    //    Claude 4 family travels on the request.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_KNOWN", MODEL, None).await,
        64_000,
        "rig's published ceiling for the Claude 4 family, not OutRig's fallback",
    );

    // 2. Another recognized identifier, whose published ceiling is higher than
    //    the fallback. Together with case 1 this is what would catch a fallback
    //    that stopped checking whether rig already had an answer: it would show
    //    up here as a ceiling *lowered* to 32768.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_HIGHER", HIGHER_CEILING_MODEL, None).await,
        128_000,
    );

    // 3. Unrecognized identifier, nothing configured: rig has no ceiling to
    //    offer, so OutRig's fallback fills the gap and the turn runs. It is
    //    deliberately not rig's silent 2048 -- see
    //    `rig_max_tokens_defaults_differ_between_constructors` below.
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
    // Recognized identifier, configured above its published 64000. The number
    // that travels is the model's, not the config's.
    assert_eq!(
        recorded_max_tokens("OUTRIG_TEST_ANTHROPIC_OVER_CEILING", MODEL, Some(128_000)).await,
        64_000,
        "the identifier's published ceiling, not the configured 128000 the API \
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
            Some(128_000),
        )
        .await,
        128_000,
    );
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

/// A turn whose only content is a thinking block is reported, not swallowed.
///
/// This is the failure that prompted the change. A think-heavy turn cut off at
/// the provider's output ceiling comes back carrying reasoning and no text.
/// Rig treats that as an ordinary success whose `output` is the empty string --
/// text parts are all `output` concatenates -- so before this, `run_turn`
/// returned `""` with no stop reason, the REPL's `if !reply.is_empty()` guard
/// printed nothing, and a minute of billed generation reached the user as
/// silence indistinguishable from outrig having ignored the prompt.
///
/// Pins all three of the properties that make it reportable: the turn is
/// recognizably silent, the reasoning is recovered rather than dropped, and the
/// report says which of the two silences this was.
#[tokio::test]
async fn a_reasoning_only_turn_is_recovered_and_reported() {
    let (addr, mut requests) = start_mock_http(vec![message(
        json!([{
            "type": "thinking",
            "thinking": "weighing the two-image split against one",
            "signature": "sig-1",
        }]),
        "max_tokens",
    )])
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
        "max_tokens",
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
