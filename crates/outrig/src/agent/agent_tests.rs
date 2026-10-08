//! The agent loop, driven against a scripted Anthropic endpoint and the
//! payload's real interpreter run on the host.
//!
//! What reached the model is read off the requests the mock recorded, so a
//! test asserts on the wire rather than on anything the loop reports about
//! itself. The e2e module at the end starts the interpreter through podman,
//! which is the one path the rest skips.

use std::future::Future;
use std::sync::{Arc, Mutex};

use rig::tool::{ToolDyn, ToolError};
use serde_json::json;

use super::budget::{ASSUMED_CONTEXT_WINDOW, Budget, DEFAULT_REPLY_RESERVE};
use super::build::{ANTHROPIC_FALLBACK_MAX_TOKENS, anthropic_model};
use super::channel::Announcer;
use super::history::{LeftOut, Manifest, Why, Window, as_sent};
use super::mock_http::{
    self, CannedResponse, MODEL, RecordedRequest, Style, check_wire, failure, submit, text_reply,
};
use super::resolve::{LlmResolveError, ResolvedCandidate, ResolvedProvider, resolve_agent};
use super::retry::RetryingHttpClient;
use super::tool::{self, SubmitPython, render, truncate_for_llm};
use super::{AgentError, PythonAgent};
use crate::config::{Config, LlmProvider};
use crate::events::{self, Events};
use crate::python::host::{Background, ExecId, Late, Outcome, Report, Unknown};
use crate::python::recovery::{GaveUp, Verdict, Waited};
use crate::python::testing::{ok, slot_freed, start_on_host, start_on_host_with, within};

const KEY: &str = "sk-ant-mock-key";

/// An identifier rig publishes no ceiling for.
const UNRECOGNIZED_MODEL: &str = "claude-3-5-sonnet-20241022";

/// Run `f` with the api-key variable `var` set.
///
/// SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
/// races. Every test uses a variable name of its own, so no two tests in this
/// binary race on one key.
fn with_key<T>(var: &str, f: impl FnOnce() -> T) -> T {
    unsafe { std::env::set_var(var, KEY) };
    let out = f();
    unsafe { std::env::remove_var(var) };
    out
}

/// `toml` loaded and validated.
fn load(toml: &str) -> Config {
    let cfg = Config::load_from_str(toml).expect("config parses");
    cfg.validate(None).expect("config validates");
    cfg
}

/// A config with an `anthropic` provider at `addr` and a `coding` agent whose
/// block carries `agent_keys`.
fn config(addr: std::net::SocketAddr, var: &str, identifier: &str, agent_keys: &str) -> Config {
    config_in(Style::Anthropic, addr, var, identifier, "", agent_keys)
}

/// A config with a provider of `style` at `addr`, a `sonnet` model on it whose
/// row carries `model_keys`, and a `coding` agent whose block carries
/// `agent_keys`.
///
/// Retries are off, so each scripted response is what the call that asked for
/// it gets: a scripted failure fails its call rather than being retried into
/// the reply after it. The retry tests turn them on.
fn config_in(
    style: Style,
    addr: std::net::SocketAddr,
    var: &str,
    identifier: &str,
    model_keys: &str,
    agent_keys: &str,
) -> Config {
    load(&format!(
        r#"
default-model = "sonnet"

[providers.claude]
style                = "{style}"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 10
retry-budget-secs    = 0

[models.sonnet]
provider   = "claude"
identifier = "{identifier}"
{model_keys}

[agents.coding]
preamble = "You write Python."
{agent_keys}
"#,
        style = style.name(),
    ))
}

/// A `coding` agent over a host-run interpreter, pointed at a mock scripted
/// with `script`.
async fn agent_over(
    var: &str,
    identifier: &str,
    agent_keys: &str,
    script: Vec<CannedResponse>,
) -> (
    PythonAgent,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) {
    agent_in(Style::Anthropic, var, identifier, "", agent_keys, script).await
}

/// [`agent_over`] against a provider of `style`, whose model row carries
/// `model_keys`.
async fn agent_in(
    style: Style,
    var: &str,
    identifier: &str,
    model_keys: &str,
    agent_keys: &str,
    script: Vec<CannedResponse>,
) -> (
    PythonAgent,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) {
    agent_with(
        Events::off(),
        style,
        var,
        identifier,
        model_keys,
        agent_keys,
        script,
    )
    .await
}

/// [`agent_in`], recording to `events`.
async fn agent_with(
    events: Events,
    style: Style,
    var: &str,
    identifier: &str,
    model_keys: &str,
    agent_keys: &str,
    script: Vec<CannedResponse>,
) -> (
    PythonAgent,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) {
    let (addr, requests) = mock_http::start(script).await;
    let cfg = config_in(style, addr, var, identifier, model_keys, agent_keys);
    (agent_of(&cfg, var, events).await, requests)
}

/// `cfg`'s `coding` agent over a host-run interpreter recording to `events`,
/// with the api-key variable `var` set.
async fn agent_of(cfg: &Config, var: &str, events: Events) -> PythonAgent {
    let interpreter = start_on_host_with(events).await;
    with_key(var, || {
        PythonAgent::with_interpreter(interpreter, cfg, Some("coding"), None)
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Send `message` on the agent's user channel, as a person typing it does.
async fn post(agent: &PythonAgent, message: &str) {
    within(agent.user_channel().send(message))
        .await
        .unwrap_or_else(|e| panic!("the message was not delivered: {e}"));
}

/// Send `message`, then run the round it starts.
async fn round(agent: &mut PythonAgent, message: &str) -> String {
    post(agent, message).await;
    within(agent.round())
        .await
        .unwrap_or_else(|e| panic!("the round failed: {e}"))
        .expect("a message was waiting, so a round ran")
}

/// Send `message`, then run the round it starts until `reached`, and drop it
/// there, as Ctrl-C at the REPL does. `biased` polls `reached` first, so a
/// round that could return in the same poll is dropped all the same.
async fn dropped_round(agent: &mut PythonAgent, message: &str, reached: impl Future) {
    post(agent, message).await;
    tokio::select! {
        biased;
        _ = within(reached) => {}
        _ = agent.round() => panic!("the round returned before it could be dropped"),
    }
}

/// Send `message`, then drop the round it starts once its second call's
/// source goes to run -- by when the first call has returned.
async fn dropped_inside_the_second(agent: &mut PythonAgent, message: &str) {
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    agent.on_submit(move |source| {
        let _ = started.send(source.to_string());
    });
    let inside_the_second = async {
        starts.recv().await.expect("the first call");
        starts.recv().await.expect("the second call");
    };
    dropped_round(agent, message, inside_the_second).await;
}

/// The text of a message's `content`, or the system prompt, whether rig sent
/// it as one string or as blocks. Blocks without text are skipped.
fn text_of(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect(),
        other => panic!("no text: {other}"),
    }
}

/// The system prompt `request` carried.
fn system_prompt(request: &RecordedRequest) -> String {
    text_of(&request.body["system"])
}

/// The messages `request` carried, as the provider received them.
fn messages(request: &RecordedRequest) -> &[serde_json::Value] {
    request.body["messages"]
        .as_array()
        .expect("a messages array")
}

/// The text of the `tool_result` for `id` in `request`, as the model reads it.
fn tool_result(request: &RecordedRequest, id: &str) -> String {
    let block = messages(request)
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
        .unwrap_or_else(|| panic!("no tool_result for {id} in {:#}", request.body));
    block["content"]
        .as_array()
        .expect("tool_result content blocks")
        .iter()
        .map(|part| part["text"].as_str().expect("a text block"))
        .collect()
}

// ---------------------------------------------------------------------------- rounds on the wire

/// The whole thesis in one exchange: the model's only tool carries source, the
/// source runs in the interpreter, and what it printed is what the model reads
/// next. The second round reads a name the first bound, and its request
/// carries the first round's exchange.
#[tokio::test]
async fn submitted_source_runs_and_its_output_reaches_the_model() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_ROUND_TRIP",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_1", "x = 6 * 7\nprint(f'x is {x}')"),
            text_reply("It is 42."),
            submit("toolu_2", "x + 1"),
            text_reply("And 43."),
        ],
    )
    .await;

    assert_eq!(round(&mut agent, "what is six sevens?").await, "It is 42.");
    assert_eq!(round(&mut agent, "and one more?").await, "And 43.");

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(
        recorded.len(),
        4,
        "two model calls per round: {recorded:#?}"
    );

    let first = &recorded[0];
    assert_eq!(first.path, "/v1/messages");
    let tools = first.body["tools"].as_array().expect("a tools array");
    assert_eq!(tools.len(), 1, "the model has one tool: {tools:#?}");
    assert_eq!(tools[0]["name"], tool::NAME);
    assert_eq!(tools[0]["input_schema"]["required"], json!(["source"]));

    assert_eq!(tool_result(&recorded[1], "toolu_1"), "x is 42\n");

    // The second round opens on the first one's four messages plus its prompt.
    let second_round = messages(&recorded[2]);
    assert_eq!(second_round.len(), 5, "{second_round:#?}");
    assert_eq!(
        tool_result(&recorded[3], "toolu_2"),
        "43\n",
        "the name the first round bound is still bound"
    );
}

/// `tool-result-max` bounds what reaches the model, and a result past it
/// arrives cut, with the marker saying so, rather than whole.
#[tokio::test]
async fn a_result_past_the_ceiling_reaches_the_model_truncated_with_the_marker() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_TRUNCATION",
        MODEL,
        "max-tokens = 4096\ntool-result-max = 1024",
        vec![submit("toolu_big", "print('a' * 5000)"), text_reply("done")],
    )
    .await;

    round(&mut agent, "print a lot").await;

    let recorded = mock_http::drain(&mut requests);
    let result = tool_result(&recorded[1], "toolu_big");
    assert!(
        result.len() <= 1024,
        "{} bytes reached the model",
        result.len()
    );
    assert!(result.starts_with("aaaa"), "{result}");
    assert!(
        result.contains("[outrig: tool result truncated]")
            && result.contains("original size: 5001 bytes"),
        "{result}"
    );
}

/// A raise whose output alone fills the bound still reaches the model as a
/// raise: the status leads the result, and it is the output that is cut.
#[tokio::test]
async fn a_raise_past_the_ceiling_still_reaches_the_model_as_a_raise() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_TRUNCATED_RAISE",
        MODEL,
        "max-tokens = 4096\ntool-result-max = 1024",
        vec![
            submit(
                "toolu_raise",
                "print('x' * 4096)\nraise RuntimeError('failed')",
            ),
            text_reply("done"),
        ],
    )
    .await;

    round(&mut agent, "fail loudly").await;

    let recorded = mock_http::drain(&mut requests);
    let result = tool_result(&recorded[1], "toolu_raise");
    assert!(result.len() <= 1024, "{} bytes", result.len());
    assert!(
        result.starts_with("[this code raised RuntimeError: failed]\n"),
        "{result}"
    );
    assert!(
        result.contains("[outrig: tool result truncated]"),
        "{result}"
    );
}

/// A model call that fails after the round ran Python keeps what it ran. The
/// interpreter still holds what it did, so a conversation that forgot it
/// would invite running it again; the error says to continue instead.
#[tokio::test]
async fn a_failed_model_call_keeps_the_python_the_round_ran() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_FAILED_AFTER_WORK",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_ran", "x = 41\nprint('ran')"),
            failure(500),
            text_reply("carried on"),
        ],
    )
    .await;

    post(&agent, "set x").await;
    let err = within(agent.round())
        .await
        .expect_err("the second model call failed");
    let err = err.to_string();
    assert!(
        err.contains("already run Python") && err.contains("rather than repeating one"),
        "{err}"
    );
    assert_eq!(round(&mut agent, "continue").await, "carried on");

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert_eq!(
        tool_result(&recorded[2], "toolu_ran"),
        "ran\n",
        "the next round's request carries the execution the failed round ran"
    );
}

/// A model call that fails before anything ran leaves the conversation as it
/// was, so resending the prompt is safe and does not repeat it.
#[tokio::test]
async fn a_failed_first_model_call_leaves_the_conversation_alone() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_FAILED_FIRST",
        MODEL,
        "max-tokens = 4096",
        vec![failure(500), text_reply("ok")],
    )
    .await;

    post(&agent, "hello").await;
    let err = within(agent.round())
        .await
        .expect_err("the model call failed")
        .to_string();
    assert!(
        err.starts_with("agent round failed:") && !err.contains("already run Python"),
        "{err}"
    );
    assert_eq!(round(&mut agent, "hello").await, "ok");

    let recorded = mock_http::drain(&mut requests);
    let sent = messages(&recorded[1]);
    assert_eq!(sent.len(), 1, "{sent:#?}");
}

/// rig reads a tool's output as JSON when it can, and an object carrying
/// `response` reaches the model as that field alone. What a program printed is
/// not a message to rig, so it arrives whole.
#[tokio::test]
async fn printed_json_reaches_the_model_whole() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_JSON",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_json", r#"print('{"response": 1, "keep": 2}')"#),
            text_reply("done"),
        ],
    )
    .await;

    round(&mut agent, "print some json").await;

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(
        tool_result(&recorded[1], "toolu_json"),
        "{\"response\": 1, \"keep\": 2}\n"
    );
}

/// Code that raises is a result the model reads, not a tool failure.
#[tokio::test]
async fn a_raise_reaches_the_model_as_its_traceback() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_RAISE",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_raise", "print('before')\n1 / 0"),
            text_reply("oops"),
        ],
    )
    .await;

    round(&mut agent, "divide").await;

    let recorded = mock_http::drain(&mut requests);
    let result = tool_result(&recorded[1], "toolu_raise");
    assert!(
        result.starts_with("[this code raised ZeroDivisionError: division by zero]\nbefore\n"),
        "{result}"
    );
    assert!(result.contains("Traceback"), "{result}");
    assert!(!result.contains("ToolCallError"), "{result}");
}

/// The tool-call cap ends the round rather than the session, names itself, and
/// keeps what the round managed for the next prompt to carry on from.
#[tokio::test]
async fn the_tool_call_cap_ends_the_round_and_keeps_it() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CAP",
        MODEL,
        "max-tokens = 4096\ntool-call-max = 1",
        vec![
            submit("toolu_a", "a = 1"),
            submit("toolu_b", "b = 2"),
            text_reply("carried on"),
        ],
    )
    .await;

    assert_eq!(
        round(&mut agent, "go").await,
        "(round ended: tool-call iteration max (1) reached)"
    );
    assert_eq!(round(&mut agent, "continue").await, "carried on");

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert_eq!(tool_result(&recorded[1], "toolu_a"), "(no output)");
    assert!(
        tool_result(&recorded[2], "toolu_b").contains("tool call not executed"),
        "the call past the cap was skipped, and the next round's request says so"
    );
}

/// A round whose future is dropped after it ran Python -- which is what Ctrl-C
/// at the REPL does to it -- keeps what it ran, as a failed model call does.
/// The next round's request carries the execution, so the model is not invited
/// to run it again.
#[tokio::test]
async fn a_round_dropped_after_it_ran_python_keeps_what_it_ran() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_DROPPED",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_ran", "x = 41\nprint('ran')"),
            text_reply("never read"),
            text_reply("carried on"),
        ],
    )
    .await;

    // Dropped once its second model call is on the wire: the Python has run
    // and its result was sent, but the round never returns. `biased` polls
    // the recorder first, and the mock records a request before it answers.
    let second_call = async {
        requests.recv().await.expect("the first model call");
        requests.recv().await.expect("the second model call");
    };
    dropped_round(&mut agent, "set x", second_call).await;
    assert_eq!(
        agent.history.len(),
        1,
        "what the round ran is kept as it is dropped, not a round later"
    );

    assert_eq!(round(&mut agent, "continue").await, "carried on");
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    assert_eq!(
        tool_result(&recorded[0], "toolu_ran"),
        "ran\n",
        "the next round's request carries the execution the dropped round ran"
    );
}

/// A turn asking for several calls runs them one at a time. Dropped while the
/// second runs, the round keeps the first's source and result: its effects
/// stand, and its outcome was already handed to the round, so no late result
/// would bring it back. Every other call in the turn is answered too, since a
/// provider refuses a call without its result -- the one in flight with a note
/// that it had not returned, the one after with a note that it never started.
#[tokio::test]
async fn a_round_dropped_mid_batch_keeps_the_calls_that_returned() {
    let batch = Style::Anthropic.batch(&[
        ("toolu_a", "x = 41\nprint('a ran')"),
        ("toolu_b", "import time\ntime.sleep(30)"),
        ("toolu_c", "print('c ran')"),
    ]);
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_DROPPED_BATCH",
        MODEL,
        "max-tokens = 4096",
        vec![batch, text_reply("carried on")],
    )
    .await;
    dropped_inside_the_second(&mut agent, "run three").await;

    assert_eq!(round(&mut agent, "continue").await, "carried on");
    let recorded = mock_http::drain(&mut requests);
    let next = recorded.last().expect("the next round's request");
    assert_eq!(tool_result(next, "toolu_a"), "a ran\n");
    assert!(
        tool_result(next, "toolu_b").contains("had not returned when the round ended"),
        "{}",
        tool_result(next, "toolu_b")
    );
    assert!(
        tool_result(next, "toolu_c").contains("not run"),
        "{}",
        tool_result(next, "toolu_c")
    );
}

/// A submission the interpreter refuses, because code a dropped round left
/// running still holds its slot, is not shown as running: it never ran.
#[tokio::test]
async fn the_observer_is_not_told_of_a_refused_submission() {
    let (mut agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_OBSERVER_REFUSED",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_slow", "import time\ntime.sleep(30)"),
            submit("toolu_next", "print('next')"),
            text_reply("refused"),
        ],
    )
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    agent.on_submit(move |source| {
        sink.lock().expect("unpoisoned").push(source.to_string());
        let _ = started.send(());
    });

    dropped_round(&mut agent, "sleep", starts.recv()).await;
    assert_eq!(round(&mut agent, "go on").await, "refused");

    assert_eq!(
        *seen.lock().expect("unpoisoned"),
        ["import time\ntime.sleep(30)"]
    );
}

/// The observer is told each submission's source, and not a call the cap
/// refuses, since that one does not run.
#[tokio::test]
async fn the_observer_is_told_each_source_that_runs() {
    let (mut agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_OBSERVER",
        MODEL,
        "max-tokens = 4096\ntool-call-max = 2",
        vec![
            submit("toolu_a", "x = 1"),
            submit("toolu_b", "print(x)"),
            submit("toolu_c", "print('past the cap')"),
        ],
    )
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    agent.on_submit(move |source| sink.lock().expect("unpoisoned").push(source.to_string()));

    round(&mut agent, "go").await;

    assert_eq!(*seen.lock().expect("unpoisoned"), ["x = 1", "print(x)"]);
}

/// The model is oriented before the agent's own preamble, and an agentless
/// session, which has no preamble of its own, is oriented all the same.
#[tokio::test]
async fn the_system_prompt_is_the_orientation_then_the_configured_preamble() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_ORIENTATION",
        MODEL,
        "max-tokens = 4096",
        vec![text_reply("hi")],
    )
    .await;
    round(&mut agent, "hello").await;
    let system = system_prompt(&mock_http::drain(&mut requests)[0]);
    assert!(
        system.starts_with("You act on this project by writing Python.")
            && system.contains("`runtime.wait` is `asyncio.wait`")
            && system.contains("`runtime.MessageAvailable`, which `except Exception` does not")
            && system.contains("`pip install` adds pure-Python packages, which import at once")
            && system.contains("Nothing compiled loads here")
            && system.contains("`help(runtime)` describes what OutRig gives you.")
            && system.ends_with("\n\nYou write Python."),
        "{system}"
    );
    // Run on the host, the interpreter has no workspace to name.
    assert!(!system.contains("working directory"), "{system}");

    let (addr, mut requests) = mock_http::start(vec![text_reply("hi")]).await;
    let var = "OUTRIG_TEST_AGENT_ORIENTATION_AGENTLESS";
    let cfg = config(addr, var, MODEL, "max-tokens = 4096");
    let interpreter = start_on_host().await;
    let mut agentless = with_key(var, || {
        PythonAgent::with_interpreter(interpreter, &cfg, None, None)
    })
    .unwrap_or_else(|e| panic!("{e}"));
    round(&mut agentless, "hello").await;
    let system = system_prompt(&mock_http::drain(&mut requests)[0]);
    assert!(
        system.starts_with("You act on this project by writing Python.")
            && !system.contains("You write Python."),
        "{system}"
    );
}

#[test]
fn the_orientation_names_the_workspace_when_there_is_one() {
    let text = super::orientation::preamble(
        Some(std::path::Path::new("/workspace")),
        None,
        Window::DEFAULT,
    );
    assert!(
        text.contains(
            "Your working directory is /workspace, which holds the project's files; its Python \
             modules import too."
        ),
        "{text}"
    );
}

/// What a startup line reports: the model row the agent runs against, never
/// an alias, and the version the interpreter itself reported.
#[tokio::test]
async fn the_agent_names_its_model_and_its_python() {
    let (agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_NAMES",
        MODEL,
        "max-tokens = 4096",
        vec![text_reply("unused")],
    )
    .await;
    assert_eq!(agent.model(), "sonnet");
    assert_eq!(
        agent.python_version(),
        start_on_host().await.version(),
        "the version the interpreter greeted with"
    );
}

// ---------------------------------------------------------------------------- the user channel

/// The text of the last user message `request` carried.
fn last_user_text(request: &RecordedRequest) -> String {
    let message = messages(request)
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap_or_else(|| panic!("no user message in {:#}", request.body));
    text_of(&message["content"])
}

/// Every recorded request, as the text on the wire.
fn wire(recorded: &[RecordedRequest]) -> String {
    recorded
        .iter()
        .map(|request| request.body.to_string())
        .collect()
}

/// A message is announced by channel and count, and `pending()` answers with
/// a count: the model learns one is waiting, and nothing it is sent carries
/// what the message says. Asserted on every request the model received.
#[tokio::test]
async fn pending_reports_a_count_without_the_body_reaching_the_model() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_PENDING",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_count", "runtime.channels['user'].pending()"),
            text_reply("one is waiting"),
        ],
    )
    .await;

    assert_eq!(
        round(&mut agent, "the secret is xyzzy").await,
        "one is waiting"
    );

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    assert_eq!(
        last_user_text(&recorded[0]),
        "[outrig] 1 message is waiting on runtime.channels[\"user\"]."
    );
    assert_eq!(tool_result(&recorded[1], "toolu_count"), "1\n");
    assert!(
        !wire(&recorded).contains("xyzzy"),
        "the body reached the model: {recorded:#?}"
    );
}

/// Receiving takes a message; being told of it does not. A message the model
/// declines to read stays queued, and the next round counts it with the new
/// one. Reading both leaves nothing, and with nothing new there is no round.
#[tokio::test]
async fn receiving_consumes_and_an_announcement_does_not() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CONSUME",
        MODEL,
        "max-tokens = 4096",
        vec![
            text_reply("not now"),
            submit(
                "toolu_read",
                "ch = runtime.channels['user']\na = await ch.receive()\nb = await ch.receive()\n\
                 print(a.body, b.body, a.sender, ch.pending())",
            ),
            text_reply("read both"),
        ],
    )
    .await;

    assert_eq!(round(&mut agent, "first").await, "not now");
    assert!(
        within(agent.round())
            .await
            .expect("no model call to fail")
            .is_none(),
        "nothing new arrived, so no round runs"
    );
    assert_eq!(round(&mut agent, "second").await, "read both");
    assert!(
        within(agent.round())
            .await
            .expect("no model call to fail")
            .is_none()
    );

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert_eq!(
        last_user_text(&recorded[1]),
        "[outrig] 2 messages are waiting on runtime.channels[\"user\"]."
    );
    assert_eq!(
        tool_result(&recorded[2], "toolu_read"),
        "first second user 0\n"
    );
}

/// Messages reach the agent's code in the order they were sent, and what it
/// sends back reaches the user in the order it sent it. Each send is counted
/// with those already waiting.
#[tokio::test]
async fn messages_and_replies_keep_their_order() {
    let (mut agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_ORDER",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit(
                "toolu_echo",
                "ch = runtime.channels['user']\nfor _ in range(3):\n    \
                 await ch.send((await ch.receive()).body.upper())",
            ),
            text_reply("echoed"),
        ],
    )
    .await;
    let user = agent.user_channel();
    for (n, text) in ["one", "two", "three"].into_iter().enumerate() {
        let waiting = within(user.send(text)).await.expect("delivered");
        assert_eq!(waiting, n + 1);
    }

    let reply = within(agent.round()).await.expect("the round ran");
    assert_eq!(reply.as_deref(), Some("echoed"));
    for want in ["ONE", "TWO", "THREE"] {
        assert_eq!(within(user.receive()).await.as_deref(), Some(want));
    }
}

/// A task the round's code leaves running can still reach the user once the
/// round is over: a send is the one way code running after the model stopped
/// writing has.
#[tokio::test]
async fn a_send_from_a_background_task_reaches_the_user_after_the_round() {
    let release = Running::new();
    let (mut agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_BACKGROUND_SEND",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit(
                "toolu_later",
                &format!(
                    "import os\nasync def later():\n    \
                     while not os.path.exists({:?}):\n        await asyncio.sleep(0.01)\n    \
                     await runtime.channels['user'].send('done later')\n\
                     task = asyncio.create_task(later())",
                    release.path()
                ),
            ),
            text_reply("started it"),
        ],
    )
    .await;
    let user = agent.user_channel();

    assert_eq!(round(&mut agent, "start it").await, "started it");
    std::fs::write(release.path(), b"").expect("release the task");
    assert_eq!(within(user.receive()).await.as_deref(), Some("done later"));
}

/// Taking what is waiting never waits, even behind a receive another clone
/// has outstanding: messages arriving now are that receive's to take.
#[tokio::test]
async fn taking_what_is_waiting_does_not_wait_behind_a_listener() {
    let (agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_SNAPSHOT",
        MODEL,
        "max-tokens = 4096",
        vec![text_reply("unused")],
    )
    .await;
    let user = agent.user_channel();
    let listener = user.clone();
    let listening = listener.receive();
    tokio::pin!(listening);
    assert!(futures_util::poll!(&mut listening).is_pending());

    assert!(user.receive_waiting().is_empty());
}

/// A message sent while the round's code runs is announced at the head of the
/// next result the model reads, by count -- counting the one that opened the
/// round, still unread -- and survives a result cut to the ceiling. The model
/// has then been told, so no round follows for it.
#[tokio::test]
async fn a_message_arriving_mid_round_is_announced_in_the_next_result() {
    let running = Running::new();
    let release = Running::new();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_MID_ROUND",
        MODEL,
        "max-tokens = 4096\ntool-result-max = 1024",
        vec![
            submit(
                "toolu_wait",
                &running.then(&format!(
                    "import os\nwhile not os.path.exists({:?}):\n    \
                     await asyncio.sleep(0.01)\nprint('a' * 5000)",
                    release.path()
                )),
            ),
            text_reply("told"),
        ],
    )
    .await;
    let user = agent.user_channel();
    let meanwhile = async {
        running.reached().await;
        user.send("the password is xyzzy").await.expect("delivered");
        std::fs::write(release.path(), b"").expect("release the code");
    };
    let (reply, ()) = tokio::join!(round(&mut agent, "go"), within(meanwhile));
    assert_eq!(reply, "told");
    assert!(
        within(agent.round())
            .await
            .expect("no model call to fail")
            .is_none(),
        "the model was told in the result"
    );

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    let result = tool_result(&recorded[1], "toolu_wait");
    assert!(
        result.starts_with("[2 messages are waiting on runtime.channels[\"user\"]]\naaa"),
        "{result}"
    );
    assert!(
        result.len() <= 1024 && result.contains("[outrig: tool result truncated]"),
        "{} bytes: {result}",
        result.len()
    );
    assert!(!wire(&recorded).contains("xyzzy"));
}

/// A message sent while the round's code waits in `runtime.wait` ends the
/// wait and not the work: the model reads that a message waits and why its
/// code stopped, the code it runs next reads the message and finds the
/// operation still running, and all of it is one round.
#[tokio::test]
async fn a_message_ends_a_wait_and_the_round_goes_on() {
    let parked = Running::new();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_REDIRECT",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit(
                "toolu_wait",
                &format!(
                    "await runtime.channels['user'].receive()\n\
                     forever = asyncio.create_task(asyncio.Event().wait(), name='forever')\n\
                     {}\ndone, pending = await runtime.wait({{forever}})",
                    parked.when_parked()
                ),
            ),
            submit(
                "toolu_read",
                "print((await runtime.channels['user'].receive()).body, forever.done())",
            ),
            text_reply("redirected"),
        ],
    )
    .await;
    let user = agent.user_channel();
    let meanwhile = async {
        parked.reached().await;
        user.send("change of plan").await.expect("delivered");
    };
    let (reply, ()) = tokio::join!(round(&mut agent, "wait for it"), within(meanwhile));
    assert_eq!(reply, "redirected");
    assert!(
        within(agent.round())
            .await
            .expect("no model call to fail")
            .is_none(),
        "the model was told in the result, so no round follows"
    );

    // The mock repeats its last answer, so only the count would show a
    // second round.
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    let waited = tool_result(&recorded[1], "toolu_wait");
    assert!(
        waited.starts_with(
            "[1 message is waiting on runtime.channels[\"user\"]]\n[this code raised \
             MessageAvailable: input is waiting on runtime.channels[\"user\"]"
        ),
        "{waited}"
    );
    assert_eq!(
        tool_result(&recorded[2], "toolu_read"),
        "change of plan False\n"
    );
}

/// A round whose model call fails before it ran anything leaves the model
/// unaware of what it announced, so the next round announces it again -- with
/// nothing new sent in between.
#[tokio::test]
async fn an_announcement_the_model_never_read_is_made_again() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_ANNOUNCE_AGAIN",
        MODEL,
        "max-tokens = 4096",
        vec![failure(500), text_reply("ok")],
    )
    .await;

    post(&agent, "hello").await;
    within(agent.round())
        .await
        .expect_err("the model call failed");
    let reply = within(agent.round())
        .await
        .expect("the second call answers");
    assert_eq!(reply.as_deref(), Some("ok"));

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    assert_eq!(
        last_user_text(&recorded[1]),
        "[outrig] 1 message is waiting on runtime.channels[\"user\"]."
    );
}

/// A call whose code has finished is not lost when its round is dropped while
/// the interpreter is asked what waits -- the one thing a call still awaits
/// after its code reports. Its outcome reaches the model as a late result, as
/// one nobody was waiting for does.
#[tokio::test]
async fn a_round_dropped_while_asking_what_waits_keeps_the_calls_outcome() {
    let asked = Running::new();
    // The interpreter's next question about what waits goes unanswered, once,
    // and marks the moment it arrives.
    let withhold = format!(
        "import __main__\n\
         def withheld(kernel, request_id, message):\n    \
         __main__._ROUTES['pending'] = __main__._pending\n    \
         open({:?}, 'w').close()\n\
         __main__._ROUTES['pending'] = withheld\n\
         print('the outcome')",
        asked.path()
    );
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_DROPPED_ASKING",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_withheld", &withhold),
            submit("toolu_next", "1 + 1"),
            text_reply("carried on"),
        ],
    )
    .await;

    dropped_round(&mut agent, "go", asked.reached()).await;
    assert_eq!(round(&mut agent, "continue").await, "carried on");

    let recorded = mock_http::drain(&mut requests);
    let next = tool_result(recorded.last().expect("a request"), "toolu_next");
    assert!(
        next.contains("has since finished: it ran to completion") && next.contains("the outcome"),
        "{next}"
    );
}

// ---------------------------------------------------------------------------- the conversation

/// Where the first message carrying `needle` sits in `request`, if one does.
fn position(request: &RecordedRequest, needle: &str) -> Option<usize> {
    messages(request)
        .iter()
        .position(|message| message.to_string().contains(needle))
}

/// A window of the first round and the one before the round in progress, so
/// a conversation of four rounds already leaves one out.
const NARROW: Window = Window {
    first: 1,
    recent: 1,
};

/// The Python that finds the turn a result mentioning `7f3a` came back in,
/// written so that the source never says what the result did.
const FIND_THE_NEEDLE: &str =
    "[t for t in runtime.history.turns if any('7f3a' in c.result for c in t.calls)]";

/// An agent on the [`NARROW`] window, three rounds into `script`, and what
/// those rounds sent.
async fn three_narrow_rounds(
    var: &str,
    script: Vec<CannedResponse>,
) -> (
    PythonAgent,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
    Vec<RecordedRequest>,
) {
    let (mut agent, mut requests) = agent_over(var, MODEL, "max-tokens = 4096", script).await;
    agent.history.set_window(NARROW);
    for message in ["one", "two", "three"] {
        round(&mut agent, message).await;
    }
    let earlier = mock_http::drain(&mut requests);
    (agent, requests, earlier)
}

/// Three rounds' script, the second of which read back a needle: by the
/// fourth, on the narrow window, the second is out of view.
fn needle_rounds(fourth: Vec<CannedResponse>) -> Vec<CannedResponse> {
    let mut script = vec![
        text_reply("first round"),
        submit("toolu_needle", "print('needle-' + '7f3a')"),
        text_reply("second round"),
        text_reply("third round"),
    ];
    script.extend(fourth);
    script
}

/// Reading the whole conversation from Python costs the model nothing: a round
/// the view has left out is found by code, and only what that code printed is
/// sent. Asserted on the wire, where the needle never appears.
#[tokio::test]
async fn a_scan_of_the_history_costs_no_context() {
    let (mut agent, mut requests, earlier) = three_narrow_rounds(
        "OUTRIG_TEST_AGENT_SCAN",
        needle_rounds(vec![
            submit("toolu_scan", &format!("[t.id for t in {FIND_THE_NEEDLE}]")),
            text_reply("found it"),
        ]),
    )
    .await;
    assert!(
        wire(&earlier).contains("needle-7f3a"),
        "the needle was read back once, in its own round"
    );

    assert_eq!(round(&mut agent, "find it").await, "found it");
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    // Nothing reads the messages, so they pile up; the note is what matters.
    let opening = last_user_text(&recorded[0]);
    assert!(
        opening.ends_with(". 2 earlier turns are not shown; runtime.history has them."),
        "{opening}"
    );
    assert_eq!(
        tool_result(&recorded[1], "toolu_scan"),
        "[1]\n",
        "the scan found the second round's first turn"
    );
    assert!(
        !wire(&recorded).contains("needle-7f3a"),
        "the scanned turn reached the model: {recorded:#?}"
    );
}

/// A turn the agent's code promotes is sent from the next model call on --
/// the same round's, since the code ran in it -- and in its original place:
/// after the first round, before the one the window keeps, not at the end.
#[tokio::test]
async fn a_promoted_turn_is_sent_in_its_place_from_the_next_call() {
    let (mut agent, mut requests, _) = three_narrow_rounds(
        "OUTRIG_TEST_AGENT_PROMOTE",
        needle_rounds(vec![
            submit(
                "toolu_promote",
                &format!("runtime.context.promote({FIND_THE_NEEDLE})"),
            ),
            text_reply("promoted"),
        ]),
    )
    .await;

    assert_eq!(round(&mut agent, "bring it back").await, "promoted");
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    assert_eq!(position(&recorded[0], "needle-7f3a"), None);

    let after = &recorded[1];
    // The first round's two messages, the promoted turn's three, the third
    // round's two, and this round's call so far.
    assert_eq!(
        messages(after).len(),
        2 + 3 + 2 + 3,
        "{:#?}",
        messages(after)
    );
    let first = position(after, "first round").expect("the first round");
    let needle = position(after, "needle-7f3a").expect("the promoted turn");
    let third = position(after, "third round").expect("the third round");
    assert!(
        first < needle && needle < third,
        "in its place: {first} < {needle} < {third}"
    );
    assert_eq!(
        position(after, "second round"),
        None,
        "only the turn promoted, not the rest of its round"
    );
}

/// The view is sent on every model call of a round, not only the first: rig
/// holds the round alone, so a call the view was not applied to would lose
/// everything before it, and one it was applied to wrongly would regain what
/// the window leaves out.
#[tokio::test]
async fn the_view_is_sent_on_every_call_of_a_round() {
    let (mut agent, mut requests, _) = three_narrow_rounds(
        "OUTRIG_TEST_AGENT_EVERY_CALL",
        vec![
            text_reply("first round"),
            text_reply("pruned-bd81"),
            text_reply("third round"),
            submit("toolu_a", "a = 1"),
            submit("toolu_b", "b = 2"),
            text_reply("done"),
        ],
    )
    .await;

    round(&mut agent, "go").await;
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    let view = &messages(&recorded[0])[..4];
    for (n, request) in recorded.iter().enumerate() {
        let sent = messages(request);
        assert_eq!(sent.len(), 5 + 2 * n, "call {n}: {sent:#?}");
        assert_eq!(&sent[..4], view, "call {n} opens on the same view");
        assert_eq!(position(request, "pruned-bd81"), None, "call {n}");
    }
    assert!(text_of(&view[1]["content"]).contains("first round"));
    assert!(text_of(&view[3]["content"]).contains("third round"));
}

/// Nothing rig hands back is spliced against the store, so a round ending
/// in any of the ways that once did so leaves every turn in it once and
/// brings back none the view left out: the tool-call cap, whose error carries
/// the round; a model call failing after Python ran; and a round dropped
/// mid-call.
#[tokio::test]
async fn a_pruned_view_does_not_bring_back_what_it_left_out() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_NO_RESURRECTION",
        MODEL,
        "max-tokens = 4096\ntool-call-max = 1",
        vec![
            text_reply("first round"),
            text_reply("second round"),
            submit("toolu_a", "a = 1"),
            submit("toolu_b", "b = 2"),
            submit("toolu_c", "c = 3"),
            failure(500),
            submit("toolu_d", "d = 4"),
            text_reply("never read"),
            text_reply("done"),
        ],
    )
    .await;
    agent.history.set_window(NARROW);
    round(&mut agent, "one").await;
    round(&mut agent, "two").await;
    assert_eq!(
        round(&mut agent, "capped").await,
        "(round ended: tool-call iteration max (1) reached)"
    );
    assert_eq!(agent.history.len(), 4, "the capped round's two turns, once");

    post(&agent, "fails").await;
    within(agent.round())
        .await
        .expect_err("the second model call failed");
    assert_eq!(agent.history.len(), 5, "the failed round's finished turn");

    mock_http::drain(&mut requests);
    let second_call = async {
        requests.recv().await.expect("the first model call");
        requests.recv().await.expect("the second model call");
    };
    dropped_round(&mut agent, "dropped", second_call).await;
    assert_eq!(agent.history.len(), 6, "the dropped round's finished turn");

    round(&mut agent, "done?").await;
    assert_eq!(agent.history.len(), 7);
    let last = mock_http::drain(&mut requests)
        .pop()
        .expect("the last round's request");
    // The first round, the dropped round's turn, and this round's prompt.
    assert_eq!(messages(&last).len(), 2 + 3 + 1, "{:#?}", messages(&last));
    for gone in ["second round", "toolu_a", "toolu_b", "toolu_c"] {
        assert_eq!(position(&last, gone), None, "{gone} came back");
    }
    assert_eq!(tool_result(&last, "toolu_d"), "(no output)");
    assert!(
        last_user_text(&last).ends_with("4 earlier turns are not shown; runtime.history has them."),
        "{}",
        last_user_text(&last)
    );
}

/// The window the model is told about is the one it gets: the first two
/// rounds and the six before the current one, so the tenth round is the first
/// to leave one out, and says so.
#[tokio::test]
async fn the_default_window_leaves_out_the_third_round_of_ten() {
    let script = (1..=10)
        .map(|n| text_reply(&format!("round-{n}.")))
        .collect();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_WINDOW",
        MODEL,
        "max-tokens = 4096",
        script,
    )
    .await;
    for n in 1..=10 {
        round(&mut agent, &format!("message {n}")).await;
    }
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 10);

    let ninth = &recorded[8];
    assert_eq!(
        messages(ninth).len(),
        2 * 8 + 1,
        "the ninth is sent everything"
    );
    assert!(
        !last_user_text(ninth).contains("earlier turn"),
        "{}",
        last_user_text(ninth)
    );

    let tenth = &recorded[9];
    assert_eq!(messages(tenth).len(), 2 * 8 + 1);
    assert_eq!(position(tenth, "round-3."), None);
    for kept in [1, 2, 4, 9] {
        assert!(
            position(tenth, &format!("round-{kept}.")).is_some(),
            "round {kept}"
        );
    }
    let opening = last_user_text(tenth);
    assert!(
        opening.ends_with(". 1 earlier turn is not shown; runtime.history has it."),
        "{opening}"
    );
}

/// The interrupt history loss `outrig run` had until 0.2.1, in `run-new`: interrupting a
/// round -- which drops its future, while the model is being called or while
/// Python runs -- no longer takes the conversation with it. The store owns
/// it, not the round.
#[tokio::test]
async fn run_new_keeps_the_conversation_when_a_round_is_interrupted() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_INTERRUPTED",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_first", "x = 1\nprint('first')"),
            text_reply("set"),
            // Dropped while the model is called.
            text_reply("never read"),
            // Dropped while its Python runs.
            submit("toolu_slow", "print('started')\nawait asyncio.sleep(3600)"),
            text_reply("carried on"),
        ],
    )
    .await;
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    agent.on_submit(move |source| {
        let _ = started.send(source.to_string());
    });
    assert_eq!(round(&mut agent, "set x").await, "set");
    starts.recv().await.expect("the first round's call");
    mock_http::drain(&mut requests);

    dropped_round(
        &mut agent,
        "interrupted while the model is called",
        requests.recv(),
    )
    .await;
    dropped_round(&mut agent, "interrupted while Python runs", starts.recv()).await;

    assert_eq!(round(&mut agent, "still there?").await, "carried on");
    let last = mock_http::drain(&mut requests)
        .pop()
        .expect("the last round's request");
    assert_eq!(
        tool_result(&last, "toolu_first"),
        "first\n",
        "run-new's loop (`PythonAgent::round`) lost the conversation to an interrupted round"
    );
    assert!(
        tool_result(&last, "toolu_slow").contains("had not returned when the round ended"),
        "{}",
        tool_result(&last, "toolu_slow")
    );
    // The first round's four, the interrupted Python's three, and this prompt.
    assert_eq!(messages(&last).len(), 4 + 3 + 1, "{:#?}", messages(&last));
}

// ---------------------------------------------------------------------------- the budget

/// Every manifest `agent`'s model calls are assembled with, from now on.
fn manifests(agent: &PythonAgent) -> Arc<Mutex<Vec<Manifest>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    agent
        .history
        .on_manifest(move |manifest| record.lock().expect("manifests").push(manifest.clone()));
    seen
}

/// How many times `needle` appears in the messages `request` carried.
fn count(request: &RecordedRequest, needle: &str) -> usize {
    request.body["messages"].to_string().matches(needle).count()
}

/// Every source the agent's interpreter accepted, in order, from now on.
fn sources(agent: &mut PythonAgent) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    agent.on_submit(move |source| record.lock().expect("sources").push(source.to_string()));
    seen
}

/// The failure this task exists to remove: a turn too large for the model's
/// window used to be sent anyway, refused, and resent every round after. Now
/// the call is not made; the round ends naming the turn, keeps it, and the
/// next round leaves it out and goes on. One execution's output is at most 16
/// KiB, about 5,500 tokens, so the window here leaves less room than that.
#[tokio::test]
async fn a_turn_too_large_for_the_window_ends_its_round_and_later_rounds_go_on() {
    let (mut agent, mut requests) = agent_in(
        Style::Anthropic,
        "OUTRIG_TEST_AGENT_TOO_LARGE",
        MODEL,
        "context-window = 7000",
        "max-tokens = 1024",
        vec![
            submit("toolu_big", "print('x' * 60000)"),
            text_reply("two"),
            text_reply("three"),
        ],
    )
    .await;
    let reply = round(&mut agent, "one").await;
    assert!(
        reply.starts_with("(round ended: turn 0 of round 1 -- the model's last call and what it returned -- is about "),
        "{reply}"
    );
    for needle in [
        "7000-token context window",
        "1024 reserved for the reply",
        "stays in runtime.history",
        "Lower tool-result-max or max-tokens",
    ] {
        assert!(reply.contains(needle), "{needle:?} in {reply}");
    }
    assert_eq!(agent.history.len(), 1, "the turn is kept");
    assert_eq!(
        mock_http::drain(&mut requests).len(),
        1,
        "the call was not made"
    );

    assert_eq!(round(&mut agent, "two").await, "two");
    let second = mock_http::drain(&mut requests).pop().expect("round two");
    assert_eq!(count(&second, "xxxxxxxxxx"), 0, "{:#}", second.body);
    let opening = last_user_text(&second);
    assert!(
        opening.ends_with(". 1 earlier turn is not shown; runtime.history has it."),
        "{opening}"
    );
    assert_eq!(round(&mut agent, "three").await, "three");
    assert_eq!(mock_http::drain(&mut requests).len(), 1);
}

/// A turn promoted while the window already holds it is sent once, and in its
/// place; promoting it twice is promoting it once; promoted out of order,
/// turns go in order.
#[tokio::test]
async fn a_promoted_turn_inside_the_window_is_sent_once_and_in_order() {
    let (mut agent, mut requests, _) = three_narrow_rounds(
        "OUTRIG_TEST_AGENT_PROMOTE_TWICE",
        needle_rounds(vec![
            // Turn 3 is the third round's, inside the window; turn 1 is the
            // needle's, outside it.
            submit(
                "toolu_promote",
                "runtime.context.promote(3)\nruntime.context.promote(1, 3)",
            ),
            text_reply("promoted"),
        ]),
    )
    .await;
    let seen = manifests(&agent);
    assert_eq!(round(&mut agent, "promote").await, "promoted");
    let after = mock_http::drain(&mut requests)
        .pop()
        .expect("the second call");
    assert_eq!(count(&after, "third round"), 1, "{:#}", after.body);
    assert_eq!(count(&after, "needle-7f3a"), 1, "{:#}", after.body);
    let first = position(&after, "first round").expect("the first round");
    let needle = position(&after, "needle-7f3a").expect("the promoted turn");
    let third = position(&after, "third round").expect("the third round");
    assert!(
        first < needle && needle < third,
        "{first} < {needle} < {third}"
    );

    let manifest = seen
        .lock()
        .expect("manifests")
        .last()
        .cloned()
        .expect("a call");
    let why: Vec<(usize, Why)> = manifest
        .carried
        .into_iter()
        .filter(|(id, _)| *id < 4)
        .collect();
    assert_eq!(
        why,
        [(0, Why::First), (1, Why::Promoted), (3, Why::Promoted)]
    );
}

/// A demotion takes effect from the next model call, in the same round, and a
/// turn promoted again is sent again.
#[tokio::test]
async fn a_demoted_turn_leaves_the_next_call() {
    let (mut agent, mut requests, _) = three_narrow_rounds(
        "OUTRIG_TEST_AGENT_DEMOTE",
        needle_rounds(vec![
            submit("toolu_promote", "runtime.context.promote(1)"),
            submit("toolu_demote", "runtime.context.demote(1)"),
            submit("toolu_again", "runtime.context.promote(1)"),
            text_reply("done"),
        ]),
    )
    .await;
    round(&mut agent, "go").await;
    let recorded = mock_http::drain(&mut requests);
    let needles: Vec<usize> = recorded.iter().map(|r| count(r, "needle-7f3a")).collect();
    assert_eq!(needles, [0, 1, 0, 1]);
}

/// A turn is promotable once it has committed and not before: the call in
/// flight is not a turn yet, and by the next call it is.
#[tokio::test]
async fn the_turn_in_flight_is_promotable_once_it_commits() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_IN_FLIGHT",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_early", "runtime.context.promote(0)"),
            submit(
                "toolu_late",
                "runtime.context.promote(0)\nprint('promoted')",
            ),
            text_reply("done"),
        ],
    )
    .await;
    round(&mut agent, "go").await;
    let last = mock_http::drain(&mut requests)
        .pop()
        .expect("the last call");
    assert!(
        tool_result(&last, "toolu_early").contains("ValueError: no finished turn has id 0"),
        "{}",
        tool_result(&last, "toolu_early")
    );
    assert_eq!(tool_result(&last, "toolu_late"), "promoted\n");
}

/// A round that ends while its calls run keeps the ones that returned and a
/// turn marked incomplete -- in the store the agent's code reads -- and
/// nothing is run again for it: the only source that runs afterwards is the
/// next round's own.
#[tokio::test]
async fn a_round_ended_mid_batch_keeps_an_incomplete_turn_and_runs_nothing_again() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_INCOMPLETE",
        MODEL,
        "max-tokens = 4096",
        vec![
            Style::Anthropic.batch(&[
                ("toolu_a", "print('a ran')"),
                ("toolu_b", "import time\ntime.sleep(3)"),
                ("toolu_c", "print('c ran')"),
            ]),
            submit(
                "toolu_look",
                "print([(t.id, t.incomplete) for t in runtime.history.turns])",
            ),
            text_reply("looked"),
        ],
    )
    .await;
    dropped_inside_the_second(&mut agent, "run three").await;
    let sources = sources(&mut agent);

    // The second call keeps the interpreter until it finishes, which the next
    // round's submission waits out by being refused; so look once it is done.
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert_eq!(round(&mut agent, "look").await, "looked");
    let last = mock_http::drain(&mut requests)
        .pop()
        .expect("the last call");
    // Its result follows word that the call the round stopped waiting for
    // has since finished.
    let look = tool_result(&last, "toolu_look");
    assert!(look.contains("[this call]\n[(0, True)]\n"), "{look}");
    assert_eq!(
        *sources.lock().expect("sources"),
        ["print([(t.id, t.incomplete) for t in runtime.history.turns])"],
        "nothing of the ended round ran again"
    );
}

/// A model call that fails leaves the turns before it whole, and no marker:
/// there is no turn of its own to mark, and the one before it is complete.
#[tokio::test]
async fn a_failed_model_call_leaves_complete_turns_and_no_marker() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_NO_MARKER",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_ran", "print('ran')"),
            failure(500),
            submit(
                "toolu_look",
                "print([(t.id, t.incomplete) for t in runtime.history.turns])",
            ),
            text_reply("looked"),
        ],
    )
    .await;
    post(&agent, "go").await;
    let err = within(agent.round()).await.expect_err("the call failed");
    assert!(!err.to_string().contains("in a row"), "{err}");
    assert_eq!(round(&mut agent, "look").await, "looked");
    let last = mock_http::drain(&mut requests)
        .pop()
        .expect("the last call");
    assert_eq!(
        tool_result(&last, "toolu_look"),
        "[(0, False)]\n",
        "the failed round's turn, whole; this round's is not a turn until it ends"
    );
}

/// `messages` as `style`'s adapter in rig puts them on the wire.
fn on_the_wire(style: Style, messages: Vec<rig::completion::Message>) -> Vec<serde_json::Value> {
    use rig::providers::{anthropic, openai};
    match style {
        Style::Anthropic => messages
            .into_iter()
            .map(|m| anthropic::completion::Message::try_from(m).expect("converts"))
            .map(|m| serde_json::to_value(m).expect("serializes"))
            .collect(),
        Style::OpenAi => messages
            .into_iter()
            .flat_map(|m| Vec::<openai::completion::Message>::try_from(m).expect("converts"))
            .map(|m| serde_json::to_value(m).expect("serializes"))
            .collect(),
    }
}

/// The messages `request` carried, without the system prompt OpenAI's
/// protocol carries among them.
fn conversation(style: Style, request: &RecordedRequest) -> Vec<serde_json::Value> {
    let skip = usize::from(style == Style::OpenAi);
    messages(request)[skip..].to_vec()
}

/// A session that cuts the conversation every way the view can: a turn a
/// promotion brings back without the rest of its round, a round cut short by
/// the cap followed by the next opening, the budget dropping this round's
/// earlier turn, and a round ended mid-batch leaving an incomplete turn.
async fn every_cut(style: Style, var: &str) -> (PythonAgent, Vec<RecordedRequest>, Vec<Manifest>) {
    every_cut_recorded(style, var, Events::off()).await
}

/// [`every_cut`], recording to `events`.
async fn every_cut_recorded(
    style: Style,
    var: &str,
    events: Events,
) -> (PythonAgent, Vec<RecordedRequest>, Vec<Manifest>) {
    let (mut agent, mut requests) = agent_with(
        events,
        style,
        var,
        MODEL,
        "",
        "max-tokens = 4096\ntool-call-max = 2",
        vec![
            // 1: turn 0.
            style.text("first"),
            // 2: turns 1 to 3; turn 2 opens on a call, mid-round.
            style.submit("call_a", "a = 1"),
            style.submit("call_b", "b = 2"),
            style.text("second"),
            // 3: the cap stops it after turn 6's results.
            style.submit("call_c", "c = 3"),
            style.submit("call_d", "d = 4"),
            style.submit("call_e", "e = 5"),
            // 4: the promoted turn 2 follows turn 0's text.
            style.submit("call_p", "runtime.context.promote(2)"),
            style.text("promoted"),
            // 5: two large turns, of which the budget sends one at a time.
            style.submit("call_y", "print('y' * 18000)"),
            style.submit("call_z", "print('z' * 18000)"),
            style.text("fifth"),
            // 6: ended while its second call runs.
            style.batch(&[
                ("call_g", "g = 1"),
                ("call_h", "import time\ntime.sleep(2)"),
                ("call_i", "i = 1"),
            ]),
            // 7.
            style.text("done"),
        ],
    )
    .await;
    agent.history.set_window(NARROW);
    // Room for either of round 5's large turns and what else there is, not
    // both: the model's own budget, its window shrunk to leave that room.
    let real = (*agent.budget).clone();
    let window = real.reserve + u32::try_from(real.overhead).expect("small") + 10_000;
    agent.budget = Arc::new(Budget { window, ..real });
    let seen = manifests(&agent);
    for message in ["one", "two", "three", "four", "five"] {
        round(&mut agent, message).await;
    }
    dropped_inside_the_second(&mut agent, "six").await;
    round(&mut agent, "seven").await;
    let recorded = mock_http::drain(&mut requests);
    let seen = seen.lock().expect("manifests").clone();
    (agent, recorded, seen)
}

/// `history.md`'s acceptance gate: a shortened history, exercised against each
/// adapter. Every request keeps each tool call beside its result, which is
/// what every provider requires; and where roles repeat is what the manifest
/// said, which is what a strict provider would refuse.
#[tokio::test]
async fn a_shortened_history_keeps_every_call_beside_its_result_on_both_adapters() {
    for style in Style::ALL {
        let var = format!("OUTRIG_TEST_AGENT_CUTS_{}", style.name().to_uppercase());
        let (_, recorded, manifests) = every_cut(style, &var).await;
        assert_eq!(
            recorded.len(),
            manifests.len(),
            "{style:?}: a manifest a call"
        );
        let mut assistant_pairs = 0;
        let mut user_pairs = 0;
        for (n, request) in recorded.iter().enumerate() {
            let wire = check_wire(style, &request.body);
            assert!(
                wire.unpaired.is_empty(),
                "{style:?} call {n}: {wire:?} {:#}",
                request.body
            );
            assistant_pairs += wire
                .adjacent
                .iter()
                .filter(|(_, r)| r == "assistant")
                .count();
            user_pairs += wire.adjacent.iter().filter(|(_, r)| r == "user").count();
        }
        assert!(
            assistant_pairs > 0,
            "{style:?}: the promotion puts a call after text"
        );
        match style {
            // A cut after results puts the next opening after them.
            Style::Anthropic => assert!(user_pairs > 0, "{style:?}"),
            // Results are `tool` messages there, which a user message follows.
            Style::OpenAi => assert_eq!(user_pairs, 0, "{style:?}"),
        }

        // Round 5's third call left its first large turn out, and carried the
        // second -- the latest -- whole.
        let fifth = recorded
            .iter()
            .position(|r| count(r, "zzzzzzzzzz") > 0)
            .expect("round 5's last call");
        assert_eq!(count(&recorded[fifth], "yyyyyyyyyy"), 0, "{style:?}");
        let evicted = &manifests[fifth].evicted;
        assert!(
            evicted.iter().any(|(_, why)| *why == Why::Round),
            "{style:?}: {evicted:?}"
        );

        // The last request carries the incomplete turn, answered in full.
        let last = recorded.last().expect("round 7");
        let text = last.body["messages"].to_string();
        for id in ["call_g", "call_h", "call_i"] {
            assert!(text.contains(id), "{style:?}: {id}");
        }
        assert!(
            text.contains("had not returned when the round ended"),
            "{style:?}"
        );
    }
}

/// Each call's manifest, with the store, rebuilds exactly what the provider
/// received, through each adapter.
#[tokio::test]
async fn each_calls_manifest_reconstructs_what_the_provider_received() {
    for style in Style::ALL {
        let var = format!("OUTRIG_TEST_AGENT_MANIFEST_{}", style.name().to_uppercase());
        let (agent, recorded, manifests) = every_cut(style, &var).await;
        for (n, (request, manifest)) in recorded.iter().zip(&manifests).enumerate() {
            assert_eq!(manifest.call, n as u64);
            assert_eq!(manifest.budget, *agent.budget);
            let rebuilt = on_the_wire(style, agent.history.reconstruct(manifest));
            assert_eq!(
                rebuilt,
                conversation(style, request),
                "{style:?} call {n}: {manifest:#?}"
            );
        }
    }
}

/// A provider's refusal of a call whose view put one role after itself says
/// so, and where; that is the one request shape this loop sends that a strict
/// provider refuses.
#[tokio::test]
async fn a_refused_call_that_carried_a_same_role_pair_says_so() {
    for style in Style::ALL {
        let var = format!("OUTRIG_TEST_AGENT_REFUSED_{}", style.name().to_uppercase());
        let (mut agent, _requests) = agent_in(
            style,
            &var,
            MODEL,
            "",
            "max-tokens = 4096",
            vec![
                style.text("first"),
                style.submit("call_a", "a = 1"),
                style.submit("call_b", "b = 2"),
                style.text("second"),
                style.text("third"),
                style.submit("call_p", "runtime.context.promote(2)"),
                style.failure(400),
            ],
        )
        .await;
        agent.history.set_window(NARROW);
        for message in ["one", "two", "three"] {
            round(&mut agent, message).await;
        }
        post(&agent, "four").await;
        let err = within(agent.round())
            .await
            .expect_err("the provider refused")
            .to_string();
        assert!(
            err.contains(
                "The conversation it was sent left turns out, which put two of the model's \
                 replies in a row, where turn 2 begins"
            ) && err.contains("doc/reference/cli.md"),
            "{style:?}: {err}"
        );
    }
}

/// The window configured on the model row is the one each call is held to,
/// through the whole configuration path: parsed, validated, resolved, built,
/// and on the call's manifest -- with the reply's reserve the ceiling that
/// reaches the wire, lowered where the provider lowers it.
#[tokio::test]
async fn an_explicit_context_window_is_the_budget_a_call_is_held_to() {
    for (window, agent_keys, reserve) in [
        (200_000, "max-tokens = 4096", 4_096),
        // rig publishes 64 000 for this identifier and lowers to it.
        (1_000_000, "max-tokens = 999999", 64_000),
    ] {
        let (mut agent, _requests) = agent_in(
            Style::Anthropic,
            "OUTRIG_TEST_AGENT_EXPLICIT_WINDOW",
            MODEL,
            &format!("context-window = {window}"),
            agent_keys,
            vec![text_reply("ok")],
        )
        .await;
        let budget = &agent.budget;
        assert_eq!(
            (
                budget.model.as_str(),
                budget.window,
                budget.window_assumed,
                budget.reserve
            ),
            ("sonnet", window, false, reserve),
        );
        let seen = manifests(&agent);
        round(&mut agent, "go").await;
        let manifest = seen.lock().expect("manifests")[0].clone();
        assert_eq!(manifest.budget, *agent.budget, "the call was held to it");
    }
}

/// With no window on the model row, one is assumed -- the same whatever the
/// identifier says -- and the reply's reserve is at most a quarter of it.
#[tokio::test]
async fn no_context_window_assumes_one_whatever_the_identifier() {
    for (style, identifier, reserve) in [
        (Style::Anthropic, MODEL, ASSUMED_CONTEXT_WINDOW / 4),
        (
            Style::Anthropic,
            UNRECOGNIZED_MODEL,
            ASSUMED_CONTEXT_WINDOW / 4,
        ),
        (Style::OpenAi, "gpt-4o", DEFAULT_REPLY_RESERVE),
    ] {
        let (agent, _requests) = agent_in(
            style,
            "OUTRIG_TEST_AGENT_ASSUMED_WINDOW",
            identifier,
            "",
            "",
            vec![style.text("ok")],
        )
        .await;
        let budget = &agent.budget;
        assert_eq!(
            (budget.window, budget.window_assumed, budget.reserve),
            (ASSUMED_CONTEXT_WINDOW, true, reserve),
            "{identifier}"
        );
    }
}

/// A reply ceiling that fills the window is a contradiction in the file, found
/// before anything starts; a window too small for the system prompt and the
/// reply is found as the agent is built.
#[tokio::test]
async fn a_window_the_reply_or_the_prompt_fills_is_refused() {
    const VAR: &str = "OUTRIG_TEST_AGENT_WINDOW_FILLED";
    let (addr, _requests) = mock_http::start(vec![text_reply("never")]).await;
    let cfg = config_in(
        Style::Anthropic,
        addr,
        VAR,
        MODEL,
        "context-window = 4096",
        "max-tokens = 4096",
    );
    let err = with_key(VAR, || PythonAgent::check(&cfg, Some("coding"), None))
        .expect_err("the reply fills the window")
        .to_string();
    assert!(
        err.starts_with(
            "[agents.coding].max-tokens is 4096, but [models.sonnet].context-window is 4096"
        ),
        "{err}"
    );

    let cfg = config_in(
        Style::Anthropic,
        addr,
        VAR,
        MODEL,
        "context-window = 3000",
        "max-tokens = 1000",
    );
    with_key(VAR, || PythonAgent::check(&cfg, Some("coding"), None))
        .expect("nothing in the file is contradictory");
    let interpreter = start_on_host().await;
    let err = with_key(VAR, || {
        PythonAgent::with_interpreter(interpreter, &cfg, Some("coding"), None)
    })
    .err()
    .expect("no room for a round")
    .to_string();
    assert!(
        err.starts_with("model \"sonnet\": a 3000-token context window leaves about ")
            && err.contains("1000 reserved for the reply"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------- the tool

/// The tool alone, over `interpreter`, at the smallest ceiling config allows.
fn tool_over(interpreter: crate::python::host::Interpreter) -> SubmitPython {
    SubmitPython::new(interpreter.clone(), 1024, Announcer::new(interpreter))
}

/// Arguments the schema does not allow are the model's mistake to fix, so
/// they fail as arguments, and nothing is submitted.
#[tokio::test]
async fn malformed_arguments_are_a_tool_error() {
    let tool = tool_over(start_on_host().await);
    for args in [
        "",
        "{}",
        r#"{"src": "1"}"#,
        r#"{"source": "1", "extra": true}"#,
    ] {
        let result = within(tool.call(args.to_string())).await;
        assert!(
            matches!(result, Err(ToolError::JsonError(_))),
            "{args:?}: {result:?}"
        );
    }
}

fn exec_id(n: u64) -> ExecId {
    serde_json::from_value(json!(n)).expect("an id")
}

/// A bound no rendering here comes near.
const WIDE: usize = 1 << 20;

/// [`render`]'s text, where every late result it was given has room.
fn all_shown(late: Vec<Late>, outcome: &Outcome, max: usize) -> String {
    let (text, unreported) = render(late, outcome, Waited::default(), max);
    assert!(
        unreported.is_empty(),
        "{} left unreported",
        unreported.len()
    );
    text
}

#[test]
fn an_empty_result_reads_as_no_output() {
    assert_eq!(all_shown(vec![], &ok(""), WIDE), "(no output)");
    assert_eq!(all_shown(vec![], &ok("\n"), WIDE), "(no output)");
}

#[test]
fn a_result_says_what_it_dropped_and_whose_output_came_with_it() {
    let outcome = Outcome::Ok(Report {
        output: "mine".to_string(),
        dropped: 7,
        background: vec![Background {
            id: exec_id(2),
            output: "theirs\n".to_string(),
            dropped: 3,
        }],
    });
    assert_eq!(
        all_shown(vec![], &outcome, WIDE),
        "mine\n\
         [7 more bytes of output were dropped]\n\
         [output from execution 2, written after it reported]\n\
         theirs\n\
         [3 more bytes were dropped]"
    );
}

fn raised(output: &str, exception: &str) -> Outcome {
    Outcome::Error {
        report: Report {
            output: output.to_string(),
            ..Report::default()
        },
        traceback: format!("Traceback ...\n{exception}\n"),
    }
}

/// An error says so first, naming the exception, then gives its output and
/// traceback.
#[test]
fn an_error_says_it_raised_then_gives_its_output_and_traceback() {
    assert_eq!(
        all_shown(
            vec![],
            &raised("before", "ZeroDivisionError: division by zero"),
            WIDE
        ),
        "[this code raised ZeroDivisionError: division by zero]\n\
         before\n\
         Traceback ...\n\
         ZeroDivisionError: division by zero\n"
    );
}

#[test]
fn a_refusal_names_the_execution_holding_the_slot() {
    let text = all_shown(vec![], &Outcome::Refused { holder: exec_id(4) }, WIDE);
    assert!(text.contains("execution 4 has not finished"), "{text}");
    assert!(text.contains("Nothing from this call ran"), "{text}");
}

/// Neither kind of unknown may read as a failure worth retrying: nothing is
/// rolled back, so a second run of something that happened is a second
/// effect.
#[test]
fn an_unknown_outcome_says_not_to_run_it_again() {
    let exited = all_shown(
        vec![],
        &Outcome::Unknown(Unknown::Exited {
            id: exec_id(5),
            cause: "its process exited (exit status: 3)".into(),
        }),
        WIDE,
    );
    assert!(exited.contains("execution 5 is unknown"), "{exited}");
    assert!(exited.contains("exit status: 3"), "{exited}");
    assert!(exited.contains("rather than running it again"), "{exited}");

    let unresolved = all_shown(
        vec![],
        &Outcome::Unknown(Unknown::Unresolved { id: exec_id(6) }),
        WIDE,
    );
    assert!(
        unresolved.contains("execution 6 is unknown"),
        "{unresolved}"
    );
    assert!(unresolved.contains("may still be running"), "{unresolved}");
    assert!(unresolved.contains("Do not run it again"), "{unresolved}");
}

/// A result nobody was waiting for is reported with the current one, and
/// labeled, so the model can tie it to the execution it was told was unknown.
#[test]
fn a_late_result_says_whose_it_is_and_how_it_ended() {
    let late = [Late {
        id: exec_id(3),
        outcome: ok("finally\n"),
    }];
    assert_eq!(
        all_shown(late.to_vec(), &ok("now\n"), WIDE),
        "[execution 3, whose call stopped waiting for it, has since finished: it ran to \
         completion]\n\
         [this call]\n\
         now\n\
         [execution 3]\n\
         finally\n"
    );
}

/// Cutting a result to fit keeps how every execution in it ended. The status
/// is the only sign an execution raised or must not be re-run, and cutting
/// the tail of the text would take the traceback, and that sign, with it.
///
/// Eight executions' statuses fill about 730 of 1024 bytes: more than a cut
/// that kept only the head would leave before its 370-byte marker.
#[test]
fn a_cut_result_still_says_how_every_execution_ended() {
    let late: Vec<Late> = (1..=7)
        .map(|n| Late {
            id: exec_id(n),
            outcome: raised(&"y".repeat(4000), &format!("ValueError: late {n}")),
        })
        .collect();
    let text = all_shown(
        late.to_vec(),
        &raised(&"x".repeat(4000), "RuntimeError: failed"),
        1024,
    );
    assert!(text.len() <= 1024, "{} bytes", text.len());
    assert!(
        text.starts_with("[this code raised RuntimeError: failed]\n"),
        "{text}"
    );
    for n in 1..=7 {
        assert!(
            text.contains(&format!(
                "has since finished: it raised ValueError: late {n}]"
            )),
            "late {n}: {text}"
        );
    }

    let exited = all_shown(
        vec![],
        &Outcome::Unknown(Unknown::Exited {
            id: exec_id(5),
            cause: "z".repeat(100_000).into(),
        }),
        1024,
    );
    assert!(exited.len() <= 1024, "{} bytes", exited.len());
    assert!(exited.contains("rather than running it again"), "{exited}");
}

/// Late statuses that do not fit are neither cut mid-line nor dropped. Those
/// that fit are shown whole, in order; the rest are counted and handed back,
/// and the next rendering shows them.
#[test]
fn late_statuses_past_the_ceiling_are_counted_and_handed_back() {
    let late: Vec<Late> = (1..=3)
        .map(|n| Late {
            id: exec_id(n),
            outcome: raised("", &format!("ValueError: {}", n.to_string().repeat(400))),
        })
        .collect();
    let (text, unreported) = render(late, &ok("now\n"), Waited::default(), 1024);
    assert!(text.len() <= 1024, "{} bytes", text.len());
    assert_eq!(unreported.len(), 1, "{text}");
    assert_eq!(unreported[0].id, exec_id(3));
    for n in 1..=2 {
        let line =
            format!("[execution {n}, whose call stopped waiting for it, has since finished: ");
        let at = text
            .find(&line)
            .unwrap_or_else(|| panic!("execution {n}: {text}"));
        assert!(text[at..].contains("]\n"), "a whole line for {n}: {text}");
    }
    assert!(
        text.contains("[1 more results that arrived late will be reported with a later call]"),
        "{text}"
    );

    let (next, rest) = render(unreported, &ok("later\n"), Waited::default(), 1024);
    assert!(rest.is_empty());
    assert!(
        next.starts_with("[execution 3, whose call stopped waiting for it, has since finished"),
        "{next}"
    );
}

/// Through the tool: what one result has no room for leads the next, and
/// every late result is reported exactly once.
#[tokio::test]
async fn the_tool_reports_every_late_result_across_calls() {
    let late: Vec<Late> = (101..=103)
        .map(|n| Late {
            id: exec_id(n),
            outcome: raised("", &format!("ValueError: {}", "z".repeat(400))),
        })
        .collect();
    let tool = tool_over(start_on_host().await).with_unreported(late);
    let mut reported = String::new();
    for _ in 0..2 {
        let text = within(tool.call(json!({ "source": "None" }).to_string()))
            .await
            .expect("the call runs");
        assert!(text.len() <= 1024, "{} bytes", text.len());
        reported.push_str(&text);
    }
    for n in 101..=103 {
        assert_eq!(
            reported
                .matches(&format!(
                    "[execution {n}, whose call stopped waiting for it, has since finished: \
                     it raised ValueError: "
                ))
                .count(),
            1,
            "execution {n} is reported once: {reported}"
        );
    }
}

#[test]
fn truncation_leaves_a_result_at_the_ceiling_alone() {
    let exact = "a".repeat(1024);
    assert_eq!(truncate_for_llm(&exact, 1024), exact);
    assert_eq!(truncate_for_llm("", 1024), "");
}

#[test]
fn truncation_cuts_on_a_character_boundary() {
    let input = format!("{}{}", "a".repeat(900), "🙂".repeat(200));
    let output = truncate_for_llm(&input, 1024);
    assert!(output.len() <= 1024, "{}", output.len());
    assert!(output.contains("[outrig: tool result truncated]"));
}

// ---------------------------------------------------------------------------- the ceiling

/// A candidate on an Anthropic provider, with only the fields `anthropic_model`
/// reads.
fn anthropic_candidate(identifier: &str, max_tokens: Option<u32>) -> ResolvedCandidate {
    ResolvedCandidate {
        model_name: identifier.to_string(),
        model_identifier: identifier.to_string(),
        provider_name: "claude".to_string(),
        provider: ResolvedProvider::Anthropic {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: KEY.to_string(),
            request_timeout_secs: None,
            retry_budget_secs: None,
        },
        max_tokens,
        context_window: None,
    }
}

/// The ceiling handed back is the one in force, not just one the config named:
/// rig's published ceiling fills in and caps, and outrig's fallback fills in
/// where rig publishes none.
#[test]
fn the_ceiling_handed_back_is_the_one_in_force() {
    let client = rig::providers::anthropic::Client::builder()
        .api_key(KEY)
        .base_url("http://127.0.0.1:1")
        .http_client(RetryingHttpClient::default())
        .build()
        .expect("client builds");
    let ceiling = |identifier: &str, configured| {
        anthropic_model(&client, &anthropic_candidate(identifier, configured)).1
    };

    assert_eq!(ceiling(MODEL, None), 64_000);
    assert_eq!(
        ceiling(UNRECOGNIZED_MODEL, None),
        ANTHROPIC_FALLBACK_MAX_TOKENS
    );
    assert_eq!(ceiling(MODEL, Some(8_192)), 8_192);
    assert_eq!(ceiling(MODEL, Some(999_999)), 64_000);
}

/// The ceiling building settles is the one that reached the wire, including
/// when it is not the configured one. This is what makes it worth reading
/// outside construction.
#[tokio::test]
async fn the_reported_ceiling_is_the_one_on_the_wire() {
    for (var, identifier, keys, expected) in [
        (
            "OUTRIG_TEST_AGENT_CEILING_CLAMPED",
            MODEL,
            "max-tokens = 999999",
            64_000,
        ),
        (
            "OUTRIG_TEST_AGENT_CEILING_FALLBACK",
            UNRECOGNIZED_MODEL,
            "",
            ANTHROPIC_FALLBACK_MAX_TOKENS,
        ),
    ] {
        let (mut agent, mut requests) =
            agent_over(var, identifier, keys, vec![text_reply("ok")]).await;
        let head = agent.agent.model.budgets().next().expect("a head");
        assert_eq!(head.max_tokens, Some(expected), "{identifier}");
        round(&mut agent, "hi").await;
        let recorded = mock_http::drain(&mut requests);
        assert_eq!(
            recorded[0].body["max_tokens"],
            json!(expected),
            "{identifier}"
        );
    }
}

// ---------------------------------------------------------------------------- resolution

/// Two providers, each keyed by `{vars}_FIRST` and `{vars}_SECOND`, and an
/// alias over a model on each. `vars` is the test's own prefix.
fn two_provider_config(vars: &str, agent_keys: &str) -> Config {
    load(&format!(
        r#"
default-model = "head"

[providers.first]
style    = "anthropic"
base-url = "http://127.0.0.1:1"
api-key  = "${{{vars}_FIRST}}"

[providers.second]
style    = "openai"
base-url = "http://127.0.0.1:2"
api-key  = "${{{vars}_SECOND}}"

[models.head]
provider   = "first"
identifier = "claude-head"

[models.tail]
provider   = "second"
identifier = "gpt-tail"

[models.chain]
alias = ["head", "tail"]

[agents.coding]
{agent_keys}
"#
    ))
}

fn with_both_keys<T>(vars: &str, f: impl FnOnce() -> T) -> T {
    with_key(&format!("{vars}_FIRST"), || {
        with_key(&format!("{vars}_SECOND"), f)
    })
}

#[test]
fn an_agentless_session_takes_the_default_model_and_the_default_limits() {
    let vars = "OUTRIG_TEST_AGENT_RESOLVE_AGENTLESS";
    let cfg = two_provider_config(vars, "");
    let resolved = with_both_keys(vars, || resolve_agent(&cfg, None, None)).expect("resolves");
    assert_eq!(resolved.head().model_name, "head");
    assert_eq!(resolved.preamble, None);
    assert_eq!(
        resolved.tool_result_max_bytes,
        crate::config::DEFAULT_TOOL_RESULT_MAX_BYTES as usize
    );
}

#[test]
fn a_model_override_wins_over_the_agent_and_the_default() {
    let vars = "OUTRIG_TEST_AGENT_RESOLVE_OVERRIDE";
    let cfg = two_provider_config(vars, "model = \"head\"");
    let resolved = with_both_keys(vars, || resolve_agent(&cfg, Some("coding"), Some("tail")))
        .expect("resolves");
    assert_eq!(resolved.head().model_identifier, "gpt-tail");
    assert!(matches!(
        resolved.head().provider,
        ResolvedProvider::OpenAi { .. }
    ));
}

#[test]
fn an_unknown_agent_names_the_ones_that_exist() {
    let vars = "OUTRIG_TEST_AGENT_RESOLVE_UNKNOWN";
    let cfg = two_provider_config(vars, "");
    let err = with_both_keys(vars, || resolve_agent(&cfg, Some("nope"), None))
        .expect_err("an unknown agent does not resolve");
    assert!(
        matches!(
            &err,
            AgentError::Resolve(LlmResolveError::UnknownAgent { known, .. }) if known == "coding"
        ),
        "{err}"
    );
}

/// `check` is `start`'s resolution with nothing started: it passes what
/// `start` would run and fails, with the same error, on what `start` would
/// refuse.
#[tokio::test]
async fn check_agrees_with_start() {
    let vars = "OUTRIG_TEST_AGENT_CHECK";
    let cfg = two_provider_config(vars, "");
    with_both_keys(vars, || PythonAgent::check(&cfg, Some("coding"), None))
        .unwrap_or_else(|e| panic!("{e}"));

    let checked = PythonAgent::check(&cfg, Some("coding"), Some("head"))
        .expect_err("the key is unset")
        .to_string();
    let started =
        PythonAgent::with_interpreter(start_on_host().await, &cfg, Some("coding"), Some("head"))
            .err()
            .expect("the key is unset")
            .to_string();
    assert_eq!(checked, started);
    assert!(checked.contains(&format!("{vars}_FIRST")), "{checked}");
}

/// The models an agent's resolution kept, in order.
fn names(resolved: &super::resolve::ResolvedAgent) -> Vec<&str> {
    resolved
        .candidates
        .iter()
        .map(|candidate| candidate.model_name.as_str())
        .collect()
}

/// An alias resolves to every one of its models this build can reach, in
/// order -- the chain a failed call moves along. One whose key is unset is
/// left out rather than tried, and when none can be reached the error names
/// each and why.
#[test]
fn an_alias_resolves_to_each_reachable_model_in_order() {
    let vars = "OUTRIG_TEST_AGENT_RESOLVE_ALIAS";
    let cfg = two_provider_config(vars, "");
    let both = with_both_keys(vars, || resolve_agent(&cfg, None, Some("chain"))).expect("resolves");
    assert_eq!(names(&both), ["head", "tail"]);

    let second = format!("{vars}_SECOND");
    let tail_only = with_key(&second, || resolve_agent(&cfg, None, Some("chain")));
    assert_eq!(names(&tail_only.expect("resolves")), ["tail"]);

    let err = resolve_agent(&cfg, None, Some("chain")).expect_err("no key is set");
    let rendered = err.to_string();
    assert!(
        rendered.contains("no usable model for alias \"chain\"")
            && rendered.contains(&format!("{vars}_FIRST"))
            && rendered.contains(&second),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------- the event log

/// Where an event of `kind` is among `kinds`, the first after `from`.
fn at(kinds: &[String], kind: &str, from: usize) -> usize {
    kinds[from..]
        .iter()
        .position(|k| k == kind)
        .map(|n| n + from)
        .unwrap_or_else(|| panic!("no {kind} after event {from}"))
}

/// What the agent did, in the order it did it, read back from the file it is
/// in by the time `shutdown` returns: a parseable stream whose records are
/// numbered by their place in it, carrying the standard attributes and
/// nothing else. The agent's taking a message, its promotion, and its reply
/// each land between its code going to run and its code's result, and the
/// promoted turn is carried by the call after it, as promoted.
#[tokio::test]
async fn a_recorded_session_is_the_agents_timeline() {
    let dir = tempfile::tempdir().expect("a log dir");
    let (mut agent, _requests) = agent_with(
        events::opened(dir.path()).await,
        Style::Anthropic,
        "OUTRIG_TEST_AGENT_EVENTS_TIMELINE",
        MODEL,
        "",
        "max-tokens = 4096",
        vec![
            text_reply("first"),
            submit(
                "toolu_1",
                "d = await runtime.channels['user'].receive()\n\
                 runtime.context.promote(0)\n\
                 await runtime.channels['user'].send('got ' + d.body)",
            ),
            text_reply("done"),
        ],
    )
    .await;
    assert_eq!(round(&mut agent, "hello").await, "first");
    assert_eq!(round(&mut agent, "promote").await, "done");
    let user = agent.user_channel();
    assert_eq!(
        within(user.receive()).await.as_deref(),
        Some("got hello"),
        "the agent's reply"
    );
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    // Read at once: what shutdown returned after is in the file.
    let records = events::recorded(dir.path());
    let kinds = events::kinds(&records);
    assert_eq!(kinds.first().map(String::as_str), Some("agent.started"));
    assert_eq!(kinds.last().map(String::as_str), Some("agent.stopped"));
    for (n, record) in records.iter().enumerate() {
        assert_eq!(record["id"], (n + 1).to_string(), "{record}");
        assert_eq!(record["specversion"], "1.0");
        assert_eq!(record["source"], events::TEST_SOURCE);
        assert_eq!(record["datacontenttype"], "application/json");
        let mut keys: Vec<&str> = record
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "data",
                "datacontenttype",
                "id",
                "source",
                "specversion",
                "subject",
                "time",
                "type",
            ],
            "{record}"
        );
    }

    let instructions = events::of_kind(&records, "model.instructions");
    assert_eq!(instructions[0]["max_tokens"], 4096, "the ceiling in force");
    assert_eq!(instructions[0]["tools"][0]["name"], tool::NAME);
    assert_eq!(
        events::of_kind(&records, "round.started"),
        [&json!({"round": 1}), &json!({"round": 2})]
    );

    // The second round, in order.
    let submitted = at(&kinds, "exec.submitted", 0);
    let took = at(&kinds, "message.received", submitted);
    let promoted = at(&kinds, "context.promoted", took);
    let replied = at(&kinds, "message.sent", promoted);
    let completed = at(&kinds, "exec.completed", replied);
    let call = at(&kinds, "model.call", completed);
    let ended = at(&kinds, "model.round.completed", call);
    assert!(submitted < took && took < promoted && promoted < replied);
    assert!(replied < completed && completed < call && call < ended);
    assert_eq!(records[promoted]["data"], json!({"turns": [0]}));
    assert_eq!(records[completed]["data"]["status"], "ok");
    assert!(
        records[call]["data"]["carried"]
            .as_array()
            .expect("carried")
            .contains(&json!({"turn": 0, "why": "promoted"})),
        "{}",
        records[call]["data"]
    );

    // The user's message, by the id it was sent under, taken by the agent's
    // code; the agent's reply, taken by the user.
    let sent = events::of_kind(&records, "message.sent");
    let received = events::of_kind(&records, "message.received");
    assert_eq!(sent.len(), 3, "two posts and a reply: {sent:#?}");
    let hello = sent
        .iter()
        .find(|m| m["body"] == "hello")
        .expect("the first post");
    assert_eq!(hello["from"], "user");
    assert_eq!(hello["to"], "agent/primary");
    assert_eq!(
        records[took]["data"],
        json!({"message": hello["message"], "channel": "user", "from": "user", "to": "agent/primary"})
    );
    let reply = sent
        .iter()
        .find(|m| m["body"] == "got hello")
        .expect("the reply");
    assert!(
        received.contains(&&json!({
            "message": reply["message"],
            "channel": "user",
            "from": "agent/primary",
            "to": "user",
        })),
        "{received:#?}"
    );
}

/// What a round used is what the provider reported: each call's tokens from
/// the response rig hands the hook, the round's from rig's `usage`, which the
/// loop used to drop. A round's high-water input is the largest a call
/// reported.
#[tokio::test]
async fn token_usage_is_what_the_provider_reported() {
    let dir = tempfile::tempdir().expect("a log dir");
    let (mut agent, _requests) = agent_with(
        events::opened(dir.path()).await,
        Style::Anthropic,
        "OUTRIG_TEST_AGENT_EVENTS_USAGE",
        MODEL,
        "",
        "max-tokens = 4096",
        vec![
            submit("toolu_1", "1").usage(json!({
                "input_tokens": 100,
                "output_tokens": 10,
                "cache_read_input_tokens": 50,
            })),
            text_reply("done").usage(json!({"input_tokens": 300, "output_tokens": 20})),
        ],
    )
    .await;
    round(&mut agent, "count").await;
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let records = events::recorded(dir.path());
    let ended = events::of_kind(&records, "model.round.completed");
    let usage = |input, output, total, cached| {
        json!({
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": total,
            "cached_input_tokens": cached,
            "cache_creation_input_tokens": 0,
            "reasoning_tokens": 0,
        })
    };
    assert_eq!(
        *ended[0],
        json!({
            "round": 1,
            "stopped": null,
            "usage": usage(400, 30, 480, 50),
            "calls": [
                {"index": 0, "model": "sonnet", "usage": usage(100, 10, 160, 50)},
                {"index": 1, "model": "sonnet", "usage": usage(300, 20, 320, 0)},
            ],
            "input_tokens_max": 300,
        })
    );
}

/// What the `model.call` manifest `call` says its call sent, from `turns`, the
/// committed turns' messages as the log recorded them: each carried turn, then
/// the opening the call held.
fn rebuilt(
    turns: &[Vec<rig::completion::Message>],
    call: &serde_json::Value,
) -> Vec<rig::completion::Message> {
    let place = |value: &serde_json::Value, key: &str| {
        usize::try_from(value[key].as_u64().expect("a place")).expect("a place fits")
    };
    let left_out: Vec<LeftOut> = call["left_out"]
        .as_array()
        .expect("left_out")
        .iter()
        .map(|part| LeftOut {
            turn: place(part, "turn"),
            message: place(part, "message"),
            part: place(part, "part"),
        })
        .collect();
    let mut messages: Vec<rig::completion::Message> = call["carried"]
        .as_array()
        .expect("carried")
        .iter()
        .flat_map(|chosen| {
            let turn = place(chosen, "turn");
            as_sent(turn, &turns[turn], &left_out)
        })
        .collect();
    if !call["opening"].is_null() {
        messages.push(serde_json::from_value(call["opening"].clone()).expect("a message"));
    }
    messages
}

/// Every `model.call` in `records` rebuilds, from the log alone, what its
/// model received: `received` gives each model's row, its provider's style,
/// and the requests its mock recorded, in order, and each call is matched with
/// the next request to the model its budget names.
fn each_call_rebuilds(
    records: &[serde_json::Value],
    received: &[(&str, Style, &[RecordedRequest])],
) {
    let turns: Vec<Vec<rig::completion::Message>> = events::of_kind(records, "turn.committed")
        .iter()
        .map(|turn| serde_json::from_value(turn["messages"].clone()).expect("rig's messages"))
        .collect();
    let mut next = vec![0; received.len()];
    for call in events::of_kind(records, "model.call") {
        let model = call["budget"]["model"].as_str().expect("a model");
        let at = received
            .iter()
            .position(|(name, ..)| *name == model)
            .unwrap_or_else(|| panic!("no requests to {model}"));
        let (_, style, requests) = received[at];
        let request = &requests[next[at]];
        next[at] += 1;
        assert_eq!(
            on_the_wire(style, rebuilt(&turns, call)),
            conversation(style, request),
            "call {} to {model}: {call:#}",
            call["call"]
        );
    }
    for ((model, _, requests), used) in received.iter().zip(next) {
        assert_eq!(used, requests.len(), "a manifest for each call to {model}");
    }
}

/// The log alone rebuilds what each call sent the provider: every committed
/// turn's messages, in rig's own form, and each call's manifest naming which
/// it carried. Exercised across every cut the view makes, through each
/// adapter, against what the mock received.
#[tokio::test]
async fn each_call_is_rebuilt_from_the_event_log_alone() {
    for style in Style::ALL {
        let dir = tempfile::tempdir().expect("a log dir");
        let var = format!(
            "OUTRIG_TEST_AGENT_EVENTS_REBUILD_{}",
            style.name().to_uppercase()
        );
        let (agent, recorded, _) =
            every_cut_recorded(style, &var, events::opened(dir.path()).await).await;
        within(agent.shutdown())
            .await
            .unwrap_or_else(|e| panic!("{e}"));

        let records = events::recorded(dir.path());
        let mut turns: Vec<Vec<rig::completion::Message>> = Vec::new();
        let mut calls = 0;
        for (kind, record) in events::kinds(&records).iter().zip(&records) {
            let data = &record["data"];
            match kind.as_str() {
                "turn.committed" => {
                    assert_eq!(data["turn"], turns.len(), "{style:?}: turns in order");
                    turns.push(
                        serde_json::from_value(data["messages"].clone())
                            .expect("rig's own messages"),
                    );
                }
                "model.call" => {
                    assert_eq!(data["call"], calls, "{style:?}");
                    assert_eq!(
                        on_the_wire(style, rebuilt(&turns, data)),
                        conversation(style, &recorded[calls]),
                        "{style:?} call {calls}: {data:#}"
                    );
                    calls += 1;
                }
                _ => {}
            }
        }
        assert_eq!(calls, recorded.len(), "{style:?}: a manifest a call");

        let preamble = events::of_kind(&records, "model.instructions")[0]["preamble"]
            .as_str()
            .expect("a preamble")
            .to_string();
        let sent = match style {
            Style::Anthropic => system_prompt(&recorded[0]),
            Style::OpenAi => text_of(&messages(&recorded[0])[0]["content"]),
        };
        assert_eq!(
            preamble, sent,
            "{style:?}: the system prompt every call carries"
        );
        let kinds = events::kinds(&records);
        for kind in ["context.promoted", "model.round.dropped"] {
            assert!(kinds.iter().any(|k| k == kind), "{style:?}: {kind}");
        }
    }
}

/// With the mode off, there is no log: not an empty one, no file at all, and
/// nothing a whole session does writes one.
#[tokio::test]
async fn with_events_off_no_log_is_written() {
    let dir = tempfile::tempdir().expect("a log dir");
    let off = Config::load_from_str("").expect("an empty config");
    let events = super::events_for(&off, dir.path(), "test")
        .await
        .expect("off opens nothing");
    assert!(!events.is_on());
    let (mut agent, _requests) = agent_with(
        events,
        Style::Anthropic,
        "OUTRIG_TEST_AGENT_EVENTS_OFF",
        MODEL,
        "",
        "max-tokens = 4096",
        vec![submit("toolu_1", "print(1)"), text_reply("done")],
    )
    .await;
    round(&mut agent, "go").await;
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("list").count(),
        0,
        "nothing in the log directory"
    );

    let on = Config::load_from_str("[events]\nmode = \"record\"\n").expect("parses");
    let events = super::events_for(&on, dir.path(), "20260921T103000-a1b2")
        .await
        .expect("record opens the log");
    events.emit(crate::events::Event::AgentStopped {});
    events.close().await.expect("nothing lost");
    let records = events::recorded(dir.path());
    assert_eq!(records[0]["source"], "/outrig/session/20260921T103000-a1b2");
    events::released(dir.path()).await;

    // An agent started again on the session would repeat those events'
    // `source` and ids, so its start is refused instead.
    let again = super::events_for(&on, dir.path(), "20260921T103000-a1b2")
        .await
        .err()
        .expect("a second recording in one log")
        .to_string();
    assert!(again.contains("already holds a recording"), "{again}");
}

/// A round that fails says so, with what it used; one that is dropped says so
/// too; and a result cut to fit says how much was cut.
#[tokio::test]
async fn failed_dropped_and_truncated_are_recorded() {
    let dir = tempfile::tempdir().expect("a log dir");
    let (mut agent, _requests) = agent_with(
        events::opened(dir.path()).await,
        Style::Anthropic,
        "OUTRIG_TEST_AGENT_EVENTS_ENDINGS",
        MODEL,
        "",
        "max-tokens = 4096\ntool-result-max = 1024",
        vec![
            failure(500),
            submit("toolu_big", "print('x' * 5000)"),
            text_reply("cut"),
            submit("toolu_slow", "import time\ntime.sleep(1)"),
            text_reply("never"),
        ],
    )
    .await;
    post(&agent, "fail").await;
    within(agent.round()).await.expect_err("the call failed");
    assert_eq!(round(&mut agent, "big").await, "cut");
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    agent.on_submit(move |_| {
        let _ = started.send(());
    });
    dropped_round(&mut agent, "slow", starts.recv()).await;
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let records = events::recorded(dir.path());
    let failed = events::of_kind(&records, "model.round.failed");
    assert_eq!(failed.len(), 1, "{failed:#?}");
    assert!(
        failed[0]["error"]
            .as_str()
            .expect("an error")
            .contains("agent round failed"),
        "{}",
        failed[0]
    );
    let truncated = events::of_kind(&records, "tool.result.truncated");
    assert_eq!(truncated.len(), 1, "{truncated:#?}");
    assert!(truncated[0]["size"].as_u64().expect("a size") > 5000);
    assert!(truncated[0]["kept"].as_u64() < truncated[0]["max"].as_u64());
    let dropped = events::of_kind(&records, "model.round.dropped");
    assert_eq!(dropped.len(), 1, "{dropped:#?}");
    assert_eq!(
        dropped[0]["calls"].as_array().expect("calls").len(),
        1,
        "the call that started the slow code"
    );
}

// ---------------------------------------------------------------------------- retry and failover

/// [`agent_with`]'s agent against Anthropic, recording to `events`, with
/// retries on -- thirty seconds of them -- where [`config_in`] turns them off.
async fn retrying_agent(
    var: &str,
    events: Events,
    script: Vec<CannedResponse>,
) -> (
    PythonAgent,
    tokio::sync::mpsc::UnboundedReceiver<RecordedRequest>,
) {
    let (addr, requests) = mock_http::start(script).await;
    let mut cfg = config(addr, var, MODEL, "max-tokens = 4096");
    retrying(&mut cfg, "claude");
    (agent_of(&cfg, var, events).await, requests)
}

/// Turn retries on for `cfg`'s provider `provider`: thirty seconds of them.
fn retrying(cfg: &mut Config, provider: &str) {
    match cfg.providers.get_mut(provider) {
        Some(
            LlmProvider::Anthropic {
                retry_budget_secs, ..
            }
            | LlmProvider::OpenAi {
                retry_budget_secs, ..
            },
        ) => *retry_budget_secs = Some(30),
        None => panic!("no provider {provider}"),
    }
}

/// Two models, `head` and `tail`, and the alias `chain` over both, which the
/// session runs. Each is given as its provider's style, that provider's
/// address, and the keys its `[models.<name>]` row carries, and each provider
/// is its own, keyed by `{vars}_FIRST` and `{vars}_SECOND`. The head's
/// provider sets the chain's retry budget, which is none: a failed call moves
/// at once.
fn chain_config(
    vars: &str,
    (head_style, head_url, head_keys): (Style, &str, &str),
    (tail_style, tail_url, tail_keys): (Style, &str, &str),
) -> Config {
    load(&format!(
        r#"
default-model = "chain"

[providers.first]
style             = "{head_style}"
base-url          = "{head_url}"
api-key           = "${{{vars}_FIRST}}"
retry-budget-secs = 0

[providers.second]
style    = "{tail_style}"
base-url = "{tail_url}"
api-key  = "${{{vars}_SECOND}}"

[models.head]
provider   = "first"
identifier = "{MODEL}"
{head_keys}

[models.tail]
provider   = "second"
identifier = "{MODEL}"
{tail_keys}

[models.chain]
alias = ["head", "tail"]
"#,
        head_style = head_style.name(),
        tail_style = tail_style.name(),
    ))
}

/// An agent over `cfg`'s chain and a host-run interpreter, recording to
/// `events`.
async fn chained_agent(vars: &str, events: Events, cfg: &Config) -> PythonAgent {
    let interpreter = start_on_host_with(events).await;
    with_both_keys(vars, || {
        PythonAgent::with_interpreter(interpreter, cfg, None, None)
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

/// A rate-limited call waits as long as the provider asks, is made again, and
/// the round completes. The retry is recorded, not only announced: the model,
/// the attempt that failed, the wait, and why -- without the provider's body.
#[tokio::test]
async fn a_rate_limited_call_is_retried_and_the_retry_recorded() {
    let dir = tempfile::tempdir().expect("a log dir");
    let (mut agent, mut requests) = retrying_agent(
        "OUTRIG_TEST_AGENT_RETRY_RATE_LIMITED",
        events::opened(dir.path()).await,
        vec![failure(429).header("Retry-After", 1), text_reply("ok")],
    )
    .await;

    let started = std::time::Instant::now();
    assert_eq!(round(&mut agent, "hi").await, "ok");
    let waited = started.elapsed();
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert!(
        waited >= std::time::Duration::from_millis(950),
        "Retry-After: 1 is honored, not {waited:?}"
    );
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(recorded.len(), 2, "{recorded:#?}");
    assert_eq!(
        recorded[0].body, recorded[1].body,
        "a retry replays the call"
    );
    let records = events::recorded(dir.path());
    assert_eq!(
        events::of_kind(&records, "model.retry"),
        [&json!({
            "model": "sonnet",
            "attempt": 1,
            "delay": 1.0,
            "error": "HTTP 429 Too Many Requests",
        })]
    );
    assert_eq!(
        events::of_kind(&records, "model.call").len(),
        1,
        "a retry resends the view it was assembled, rather than assembling another"
    );
}

/// A `200` rig cannot use is retried below the round: the call is made again
/// with what it carried, so the Python an earlier call ran does not run again.
/// It is recorded as a retry like any other.
#[tokio::test]
async fn an_unusable_response_is_retried_without_running_python_again() {
    let dir = tempfile::tempdir().expect("a log dir");
    let (mut agent, mut requests) = retrying_agent(
        "OUTRIG_TEST_AGENT_RETRY_UNUSABLE",
        events::opened(dir.path()).await,
        vec![
            submit("toolu_1", "print('ran')"),
            mock_http::unusable(),
            text_reply("done"),
        ],
    )
    .await;
    let ran = sources(&mut agent);

    assert_eq!(round(&mut agent, "run it").await, "done");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(*ran.lock().expect("sources"), ["print('ran')"]);
    let recorded = mock_http::drain(&mut requests);
    assert_eq!(
        recorded.len(),
        3,
        "the tool call, the unusable reply, the retry"
    );
    assert_eq!(
        recorded[1].body, recorded[2].body,
        "the retried call carries what the unusable one did"
    );
    assert_eq!(tool_result(&recorded[2], "toolu_1"), "ran\n");
    let records = events::recorded(dir.path());
    let retries = events::of_kind(&records, "model.retry");
    assert_eq!(retries.len(), 1, "{retries:#?}");
    assert!(
        retries[0]["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("unusable response")),
        "{}",
        retries[0]
    );
}

/// A chain whose first model cannot be reached answers from the second, and
/// the record says so: the move, and the model that answered the call.
#[tokio::test]
async fn an_unreachable_head_moves_to_the_next_model_which_is_named_as_answering() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_UNREACHABLE";
    let (tail, mut requests) = mock_http::start(vec![text_reply("from the tail")]).await;
    // The discard port: nothing listens there, so every connection is refused.
    let cfg = chain_config(
        vars,
        (Style::Anthropic, "http://127.0.0.1:9", ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;
    assert_eq!(
        agent.model(),
        "head",
        "what every call is tried against first"
    );

    assert_eq!(round(&mut agent, "hi").await, "from the tail");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(mock_http::drain(&mut requests).len(), 1);
    let records = events::recorded(dir.path());
    let moves = events::of_kind(&records, "model.failover");
    assert_eq!(moves.len(), 1, "{moves:#?}");
    assert_eq!(
        (&moves[0]["from"], &moves[0]["to"]),
        (&json!("head"), &json!("tail"))
    );
    assert!(
        moves[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("127.0.0.1:9")),
        "{}",
        moves[0]
    );
    let ended = events::of_kind(&records, "model.round.completed");
    assert_eq!(
        ended[0]["calls"][0]["model"], "tail",
        "the call is attributed to the model that answered it, not the head"
    );
}

/// A move replays one model call against the next model, not the round around
/// it: the Python the head's earlier call ran does not run again, and the
/// moved call carries its result. Each call is named for its own model.
#[tokio::test]
async fn python_run_before_a_move_is_not_run_again() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_MID_ROUND";
    let (head, mut head_requests) =
        mock_http::start(vec![submit("toolu_1", "print('ran')"), failure(503)]).await;
    let (tail, mut tail_requests) =
        mock_http::start(vec![text_reply("finished on the tail")]).await;
    let cfg = chain_config(
        vars,
        (Style::Anthropic, &format!("http://{head}"), ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;
    let ran = sources(&mut agent);

    assert_eq!(round(&mut agent, "run it").await, "finished on the tail");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(*ran.lock().expect("sources"), ["print('ran')"]);
    assert_eq!(mock_http::drain(&mut head_requests).len(), 2);
    let moved = mock_http::drain(&mut tail_requests);
    assert_eq!(moved.len(), 1);
    assert_eq!(tool_result(&moved[0], "toolu_1"), "ran\n");
    let records = events::recorded(dir.path());
    let calls = &events::of_kind(&records, "model.round.completed")[0]["calls"];
    let models: Vec<_> = calls
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| (call["index"].clone(), call["model"].clone()))
        .collect();
    assert_eq!(
        models,
        [(json!(0), json!("head")), (json!(1), json!("tail"))]
    );
}

/// When every model fails, the round ends with each one's reason and keeps
/// the Python it ran, in the wording any failed round uses: continue rather
/// than repeat. The session goes on, and the next round carries what ran.
#[tokio::test]
async fn an_exhausted_chain_ends_the_round_and_keeps_what_it_ran() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_EXHAUSTED";
    let (head, mut head_requests) = mock_http::start(vec![
        submit("toolu_1", "x = 41\nprint('ran')"),
        failure(503),
        text_reply("carried on"),
    ])
    .await;
    let (tail, _tail_requests) = mock_http::start(vec![failure(401)]).await;
    let cfg = chain_config(
        vars,
        (Style::Anthropic, &format!("http://{head}"), ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    let mut agent = chained_agent(vars, Events::off(), &cfg).await;
    let ran = sources(&mut agent);

    post(&agent, "set x").await;
    let err = within(agent.round())
        .await
        .expect_err("every model failed")
        .to_string();
    assert!(
        err.contains("every model candidate failed; tried:"),
        "a 503 can clear, so the chain does not say terminally: {err}"
    );
    for reason in ["head", "503", "tail", "401"] {
        assert!(err.contains(reason), "{reason}: {err}");
    }
    assert!(
        err.contains("already run Python") && err.contains("rather than repeating one"),
        "{err}"
    );

    assert_eq!(round(&mut agent, "continue").await, "carried on");
    assert_eq!(*ran.lock().expect("sources"), ["x = 41\nprint('ran')"]);
    let recorded = mock_http::drain(&mut head_requests);
    assert_eq!(recorded.len(), 3, "{recorded:#?}");
    assert_eq!(
        tool_result(&recorded[2], "toolu_1"),
        "ran\n",
        "the next round carries what the failed one ran"
    );
}

/// The check `0003-12` left for this task: a call that moves to a model with
/// a smaller window is sent a view assembled for that window, not the head's.
/// The head's view carried round 1's large turn; the tail's leaves it out, and
/// the move's manifest says so, names the tail's allowance, and rebuilds what
/// the tail received. Each model's own output ceiling reaches the wire with
/// it, as building settled and the log recorded it.
#[tokio::test]
async fn a_move_to_a_smaller_window_is_sent_a_view_assembled_for_it() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_REBUDGET";
    let (head, mut head_requests) = mock_http::start(vec![
        submit("toolu_big", "print('a' * 15000)"),
        text_reply("first"),
        failure(503),
    ])
    .await;
    let (tail, mut tail_requests) = mock_http::start(vec![text_reply("second")]).await;
    let cfg = chain_config(
        vars,
        (
            Style::Anthropic,
            &format!("http://{head}"),
            "max-tokens = 4096",
        ),
        (
            Style::Anthropic,
            &format!("http://{tail}"),
            "max-tokens = 1024\ncontext-window = 6000",
        ),
    );
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;
    let ceilings: Vec<_> = agent
        .agent
        .model
        .budgets()
        .map(|budget| (budget.model.as_str(), budget.max_tokens))
        .collect();
    assert_eq!(ceilings, [("head", Some(4096)), ("tail", Some(1024))]);

    assert_eq!(round(&mut agent, "make a large result").await, "first");
    assert_eq!(round(&mut agent, "and then?").await, "second");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let at_head = mock_http::drain(&mut head_requests);
    let at_tail = mock_http::drain(&mut tail_requests);
    assert_eq!((at_head.len(), at_tail.len()), (3, 1));
    // The same call, as each model was sent it.
    let (headed, moved) = (&at_head[2], &at_tail[0]);
    tool_result(headed, "toolu_big");
    assert!(
        !moved.body.to_string().contains("toolu_big"),
        "the tail's window leaves the large turn out: {:#}",
        moved.body
    );
    assert!(
        check_wire(Style::Anthropic, &moved.body)
            .unpaired
            .is_empty()
    );
    assert_eq!(headed.body["max_tokens"], 4096);
    assert_eq!(moved.body["max_tokens"], 1024);

    let records = events::recorded(dir.path());
    let kinds = events::kinds(&records);
    assert_eq!(
        events::of_kind(&records, "model.instructions")[0]["max_tokens"],
        4096,
        "the head's ceiling, as the head's call carried it"
    );
    let failover = at(&kinds, "model.failover", 0);
    let head_call = *events::of_kind(&records[..failover], "model.call")
        .last()
        .expect("the head's manifest of the call");
    let moved_call = &records[at(&kinds, "model.call", failover)]["data"];
    assert_eq!(head_call["budget"]["model"], "head");
    assert_eq!(
        moved_call["budget"],
        json!({
            "model": "tail",
            "window": 6000,
            "window_assumed": false,
            "reserve": 1024,
            "overhead": moved_call["budget"]["overhead"],
            "max_tokens": 1024,
        })
    );
    // Turn 0 is round 1's opening, the call, and its large result.
    assert_eq!(head_call["evicted"], json!([]), "{head_call:#}");
    assert_eq!(
        moved_call["evicted"],
        json!([{"turn": 0, "why": "first"}]),
        "{moved_call:#}"
    );

    let turns: Vec<Vec<rig::completion::Message>> = events::of_kind(&records, "turn.committed")
        .iter()
        .map(|turn| serde_json::from_value(turn["messages"].clone()).expect("rig's messages"))
        .collect();
    for (call, request) in [(head_call, headed), (moved_call, moved)] {
        assert_eq!(
            on_the_wire(Style::Anthropic, rebuilt(&turns, call)),
            conversation(Style::Anthropic, request)
        );
    }
}

/// The blocks of `kind` in the messages `request` carried, as the Anthropic
/// API received them.
fn blocks<'a>(request: &'a RecordedRequest, kind: &str) -> Vec<&'a serde_json::Value> {
    messages(request)
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == kind)
        .collect()
}

/// Anthropic's own signed thinking goes back to Anthropic as it came: what
/// keeps a call to another provider valid must not strip it from the one that
/// wrote it.
#[tokio::test]
async fn signed_thinking_goes_back_to_anthropic_whole() {
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_SIGNED_THINKING",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_1", "print('ran')").with_reasoning("weighing it"),
            text_reply("done"),
        ],
    )
    .await;

    assert_eq!(round(&mut agent, "run it").await, "done");

    let recorded = mock_http::drain(&mut requests);
    assert_eq!(
        blocks(&recorded[1], "thinking"),
        [&json!({
            "type": "thinking",
            "thinking": "weighing it",
            "signature": mock_http::SIGNATURE,
        })]
    );
}

/// A call that moves from an OpenAI-compatible model to Anthropic's is sent
/// the earlier reasoning the first one wrote as Anthropic takes it: not at
/// all, since there is no signature to send it with, and a thinking block
/// without one is a request Anthropic refuses. The conversation keeps it.
#[tokio::test]
async fn reasoning_another_provider_wrote_is_not_sent_to_anthropic() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_FOREIGN_REASONING";
    let (head, mut head_requests) = mock_http::start(vec![
        Style::OpenAi
            .submit("call_1", "print('ran')")
            .with_reasoning("thinking it over"),
        Style::OpenAi.failure(503),
    ])
    .await;
    let (tail, mut tail_requests) =
        mock_http::start(vec![text_reply("finished on Anthropic")]).await;
    let cfg = chain_config(
        vars,
        (Style::OpenAi, &format!("http://{head}"), ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;

    assert_eq!(round(&mut agent, "run it").await, "finished on Anthropic");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let moved = mock_http::drain(&mut tail_requests);
    assert_eq!(moved.len(), 1);
    assert!(
        blocks(&moved[0], "thinking").is_empty(),
        "{:#}",
        moved[0].body
    );
    assert!(
        check_wire(Style::Anthropic, &moved[0].body)
            .unpaired
            .is_empty()
    );
    assert_eq!(tool_result(&moved[0], "call_1"), "ran\n");
    let records = events::recorded(dir.path());
    assert!(
        events::of_kind(&records, "turn.committed")[0]["messages"]
            .to_string()
            .contains("thinking it over"),
        "the conversation keeps what the model wrote"
    );
    let headed = mock_http::drain(&mut head_requests);
    each_call_rebuilds(
        &records,
        &[
            ("head", Style::OpenAi, &headed),
            ("tail", Style::Anthropic, &moved),
        ],
    );
}

/// The same when the Anthropic model is the head: a later call starts there
/// again, and leaves out what the OpenAI-compatible model it moved to on the
/// call before reasoned. Every call's record rebuilds what its model received.
#[tokio::test]
async fn the_anthropic_head_is_sent_none_of_the_reasoning_its_fallback_wrote() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_FOREIGN_REASONING_HEAD";
    let (head, mut head_requests) =
        mock_http::start(vec![failure(503), text_reply("finished on the head")]).await;
    let (tail, mut tail_requests) = mock_http::start(vec![
        Style::OpenAi
            .submit("call_1", "print('ran')")
            .with_reasoning("thinking it over"),
    ])
    .await;
    let cfg = chain_config(
        vars,
        (Style::Anthropic, &format!("http://{head}"), ""),
        (Style::OpenAi, &format!("http://{tail}"), ""),
    );
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;

    assert_eq!(round(&mut agent, "run it").await, "finished on the head");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let headed = mock_http::drain(&mut head_requests);
    let moved = mock_http::drain(&mut tail_requests);
    assert_eq!((headed.len(), moved.len()), (2, 1));
    assert!(
        blocks(&headed[1], "thinking").is_empty(),
        "{:#}",
        headed[1].body
    );
    assert_eq!(
        tool_result(&headed[1], "call_1"),
        "ran
"
    );
    let records = events::recorded(dir.path());
    let calls = events::of_kind(&records, "model.call");
    assert_eq!(
        calls.last().expect("the head's second call")["left_out"],
        json!([{"turn": 0, "message": 1, "part": 0}]),
        "the opening, then the reply whose reasoning is left out"
    );
    each_call_rebuilds(
        &records,
        &[
            ("head", Style::Anthropic, &headed),
            ("tail", Style::OpenAi, &moved),
        ],
    );
}

/// A request that cannot succeed as sent -- a redirect loop reqwest gives up
/// on -- is not retried: the call moves to the next model at once, with its
/// retry budget unspent.
#[tokio::test]
async fn a_redirect_loop_moves_on_without_retrying() {
    let vars = "OUTRIG_TEST_AGENT_FAILOVER_REDIRECT_LOOP";
    let (head, mut head_requests) =
        mock_http::start(vec![failure(307).header("Location", "/v1/messages")]).await;
    let (tail, _tail_requests) = mock_http::start(vec![text_reply("from the tail")]).await;
    let mut cfg = chain_config(
        vars,
        (Style::Anthropic, &format!("http://{head}"), ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    retrying(&mut cfg, "first");
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;

    assert_eq!(round(&mut agent, "hi").await, "from the tail");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let followed = mock_http::drain(&mut head_requests).len();
    assert!(
        (2..=11).contains(&followed),
        "one request and the redirects reqwest follows, once: {followed}"
    );
    let records = events::recorded(dir.path());
    assert_eq!(
        events::of_kind(&records, "model.retry"),
        Vec::<&serde_json::Value>::new()
    );
    assert_eq!(events::of_kind(&records, "model.failover").len(), 1);
}

/// Anthropic's overloaded status is temporary: the same model is asked again
/// after the wait, and the call does not move.
#[tokio::test]
async fn an_overloaded_model_is_retried_rather_than_left() {
    let vars = "OUTRIG_TEST_AGENT_RETRY_OVERLOADED";
    let (head, mut head_requests) = mock_http::start(vec![
        failure(529).header("Retry-After", 0),
        text_reply("from the head"),
    ])
    .await;
    let (tail, mut tail_requests) = mock_http::start(vec![text_reply("from the tail")]).await;
    let mut cfg = chain_config(
        vars,
        (Style::Anthropic, &format!("http://{head}"), ""),
        (Style::Anthropic, &format!("http://{tail}"), ""),
    );
    retrying(&mut cfg, "first");
    let dir = tempfile::tempdir().expect("a log dir");
    let mut agent = chained_agent(vars, events::opened(dir.path()).await, &cfg).await;

    assert_eq!(round(&mut agent, "hi").await, "from the head");
    within(agent.shutdown())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(mock_http::drain(&mut head_requests).len(), 2);
    assert!(mock_http::drain(&mut tail_requests).is_empty());
    let records = events::recorded(dir.path());
    let retries = events::of_kind(&records, "model.retry");
    assert_eq!(retries.len(), 1, "{retries:#?}");
    assert!(
        retries[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("529")),
        "{}",
        retries[0]
    );
    assert!(events::of_kind(&records, "model.failover").is_empty());
}

// ---------------------------------------------------------------------------- through podman

// ---------------------------------------------------------------------------- Ctrl-C

/// What the host did while it waited is said alongside the outcome, each thing
/// on its own. A runaway interrupt is not pinned on a target: it may have
/// ended another execution's task, or this code may have caught it, and a
/// clean run is described as one either way. A user's stop is still said when
/// the host went on to interrupt a runaway, and an automatic give-up after it
/// is not reported as the user asking twice.
#[test]
fn what_was_done_is_rendered_with_the_outcome_it_led_to() {
    let render_one = |outcome: &Outcome, waited| render(Vec::new(), outcome, waited, 4096).0;
    let interrupted = raised("", "KeyboardInterrupt: interrupted by outrig");
    let user = Waited {
        user_stopped: true,
        ..Waited::default()
    };
    let runaway = Waited {
        runaway_interrupted: true,
        ..Waited::default()
    };
    let both = Waited {
        user_stopped: true,
        ..runaway
    };

    let raised_runaway = render_one(&interrupted, runaway);
    assert!(
        raised_runaway.starts_with(
            "[this code raised KeyboardInterrupt: interrupted by outrig: the event loop"
        ),
        "{raised_runaway}"
    );
    assert!(
        raised_runaway.contains("asyncio.to_thread"),
        "{raised_runaway}"
    );

    let finished = render_one(&ok("fine\n"), runaway);
    assert!(
        finished.contains("the call then ran to completion"),
        "{finished}"
    );
    assert!(!finished.contains("not this code"), "{finished}");
    assert!(finished.ends_with("fine\n"), "{finished}");

    for outcome in [&interrupted, &ok("fine\n")] {
        let text = render_one(outcome, both);
        assert!(
            text.starts_with("[the user interrupted this call, and the event loop"),
            "{text}"
        );
        assert!(text.contains("does not stop the processes"), "{text}");
    }
    assert!(
        render_one(&ok("fine\n"), user).starts_with("[the user interrupted this call, but it ran")
    );

    let id = exec_id(7);
    let unresolved = Outcome::Unknown(Unknown::Unresolved { id });
    let exhausted = render_one(
        &unresolved,
        Waited {
            gave_up: Some(GaveUp::Runaway),
            ..both
        },
    );
    assert!(
        exhausted.contains("kept spinning through 3 interrupts"),
        "{exhausted}"
    );
    assert!(!exhausted.contains("twice"), "{exhausted}");
    let twice = render_one(
        &unresolved,
        Waited {
            gave_up: Some(GaveUp::User),
            ..user
        },
    );
    assert!(twice.contains("the user interrupted it twice"), "{twice}");

    for (verdict, says) in [
        (Verdict::Spinning, "has interrupted it"),
        (Verdict::Blocked, "blocked in a call"),
        (Verdict::Turning, "suspended on an await"),
        (Verdict::Starved, "native code"),
    ] {
        let refused = render_one(
            &Outcome::Refused { holder: id },
            Waited {
                holder: Some(verdict),
                ..Waited::default()
            },
        );
        assert!(
            refused.starts_with("Not run: execution 7 still holds"),
            "{refused}"
        );
        assert!(refused.contains(says), "{verdict:?}: {refused}");
    }
}

/// A file the code under test creates once it is running, so a press lands
/// in its body rather than before it starts.
struct Running(tempfile::TempDir);

impl Running {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("tempdir"))
    }

    fn path(&self) -> std::path::PathBuf {
        self.0.path().join("running")
    }

    /// `source`, run once the file exists.
    fn then(&self, source: &str) -> String {
        format!("open({:?}, 'w').close()\n{source}", self.path())
    }

    /// Python that creates the file once the loop turns again. Put just before
    /// an `await`, it is created once that await has suspended.
    fn when_parked(&self) -> String {
        format!(
            "asyncio.get_running_loop().call_soon(lambda: open({:?}, 'w').close())",
            self.path()
        )
    }

    /// Wait for the file to exist.
    async fn reached(&self) {
        while !self.path().exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Run `prompt` as a round, pressing Ctrl-C `presses` times once `running`
/// exists, and return the round's reply with what each press said.
async fn round_pressed(
    agent: &mut PythonAgent,
    prompt: &str,
    running: &Running,
    presses: usize,
) -> (String, Vec<Option<String>>) {
    let interrupt = agent.interrupter();
    let pressing = async {
        running.reached().await;
        (0..presses).map(|_| interrupt()).collect()
    };
    tokio::join!(round(agent, prompt), within(pressing))
}

/// A bare `await` on something that never resolves is what a user is most
/// likely to need Ctrl-C for, and what no probe can see. The press cancels it,
/// the model reads how it ended and carries on, and the slot is free for the
/// next round's code.
#[tokio::test]
async fn ctrl_c_ends_an_await_that_never_resolves_and_the_model_reads_it() {
    let running = Running::new();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CTRL_C_AWAIT",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit(
                "toolu_wait",
                &running.then("await asyncio.get_running_loop().create_future()"),
            ),
            text_reply("stopped it"),
            submit("toolu_next", "1 + 1"),
            text_reply("two"),
        ],
    )
    .await;

    let (reply, said) = round_pressed(&mut agent, "wait forever", &running, 1).await;
    assert_eq!(reply, "stopped it");
    let said = said[0].as_deref().expect("a call was waiting");
    assert!(said.starts_with("stopping execution"), "{said}");

    assert_eq!(round(&mut agent, "then add").await, "two");
    let recorded = mock_http::drain(&mut requests);
    let stopped = tool_result(&recorded[1], "toolu_wait");
    assert!(
        stopped.starts_with("[the user interrupted this call, and this code raised"),
        "{stopped}"
    );
    assert!(stopped.contains("CancelledError"), "{stopped}");
    assert!(!stopped.contains("before it started"), "{stopped}");
    assert!(stopped.contains("does not stop the processes"), "{stopped}");
    assert_eq!(tool_result(&recorded[3], "toolu_next"), "2\n");
}

/// With no Python running there is nothing for Ctrl-C to reach here, during
/// a round or at the prompt; the caller stops a round there by dropping it.
#[tokio::test]
async fn ctrl_c_with_no_python_running_does_nothing() {
    let (mut agent, _requests) = agent_over(
        "OUTRIG_TEST_AGENT_CTRL_C_IDLE",
        MODEL,
        "max-tokens = 4096",
        vec![submit("toolu_x", "x = 1"), text_reply("bound")],
    )
    .await;
    let interrupt = agent.interrupter();
    assert_eq!(interrupt(), None);
    assert_eq!(agent.stop_held(), None);
    assert_eq!(round(&mut agent, "bind").await, "bound");
    assert_eq!(interrupt(), None);
    assert_eq!(agent.stop_held(), None);
}

/// Stopping one call in a turn stops the turn: the model wrote the next call
/// before it knew the first would be stopped, so it does not run until the
/// model has read that.
#[tokio::test]
async fn a_later_call_in_a_stopped_turn_is_not_run() {
    let running = Running::new();
    let batch = mock_http::message(
        json!([
            { "type": "tool_use", "id": "toolu_wait", "name": tool::NAME,
              "input": { "source": running.then("await asyncio.get_running_loop().create_future()") } },
            { "type": "tool_use", "id": "toolu_after", "name": tool::NAME,
              "input": { "source": "after = True" } },
        ]),
        "tool_use",
    );
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CTRL_C_BATCH",
        MODEL,
        "max-tokens = 4096",
        vec![
            batch,
            text_reply("read it"),
            submit("toolu_check", "'after' in globals()"),
            text_reply("checked"),
        ],
    )
    .await;

    let (reply, _) = round_pressed(&mut agent, "two things", &running, 1).await;
    assert_eq!(reply, "read it");
    assert_eq!(round(&mut agent, "did the second run?").await, "checked");
    let recorded = mock_http::drain(&mut requests);
    let skipped = tool_result(&recorded[1], "toolu_after");
    assert!(
        skipped.contains("not run: the user interrupted"),
        "{skipped}"
    );
    assert_eq!(tool_result(&recorded[3], "toolu_check"), "False\n");
}

/// A synchronous `subprocess.run` blocks the loop, so the cancel cannot reach
/// it; the press interrupts the blocking call instead. What the child started
/// is not stopped with it, and the model is told so.
#[tokio::test]
async fn ctrl_c_on_a_blocking_run_says_what_it_leaves_running() {
    let running = Running::new();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CTRL_C_RUN",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit(
                "toolu_run",
                &running.then("import subprocess\nsubprocess.run(['sleep', '60'])"),
            ),
            text_reply("interrupted"),
        ],
    )
    .await;

    let (reply, _) = round_pressed(&mut agent, "sleep", &running, 1).await;
    assert_eq!(reply, "interrupted");
    let recorded = mock_http::drain(&mut requests);
    let result = tool_result(&recorded[1], "toolu_run");
    assert!(
        result.contains("KeyboardInterrupt: interrupted by outrig"),
        "{result}"
    );
    assert!(result.contains("does not stop the processes"), "{result}");
}

/// Pressed twice, the call stops waiting: the model is told the outcome is
/// unknown and not to run it again, and the execution keeps the slot, so the
/// next submission is refused behind it. The stop with nobody waiting --
/// Ctrl-C at the prompt -- cancels it again (#468), and the round after runs
/// its code, with how the holder ended reported ahead of the result.
#[tokio::test]
async fn ctrl_c_twice_stops_waiting_and_a_stop_at_the_prompt_ends_it() {
    let running = Running::new();
    let caught = running.then(
        "try:\n    await asyncio.get_running_loop().create_future()\n\
         except asyncio.CancelledError:\n    pass\n\
         await asyncio.get_running_loop().create_future()",
    );
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_CTRL_C_TWICE",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_stubborn", &caught),
            text_reply("gave up"),
            submit("toolu_next", "1"),
            text_reply("refused"),
            submit("toolu_again", "2"),
            text_reply("ran"),
        ],
    )
    .await;

    let (reply, said) = round_pressed(&mut agent, "wait", &running, 2).await;
    assert_eq!(reply, "gave up");
    let second = said[1].as_deref().expect("a call was still waiting");
    assert!(
        second.starts_with("no longer waiting for execution"),
        "{second}"
    );
    assert_eq!(round(&mut agent, "try again").await, "refused");

    let said = agent
        .stop_held()
        .expect("an execution was left holding the interpreter");
    assert!(said.starts_with("stopping execution"), "{said}");
    slot_freed(&agent.interpreter).await;
    assert_eq!(agent.stop_held(), None);
    assert_eq!(round(&mut agent, "once more").await, "ran");

    let recorded = mock_http::drain(&mut requests);
    let unknown = tool_result(&recorded[1], "toolu_stubborn");
    assert!(unknown.contains("interrupted it twice"), "{unknown}");
    assert!(unknown.contains("Do not run it again"), "{unknown}");
    let refused = tool_result(&recorded[3], "toolu_next");
    assert!(refused.starts_with("Not run: execution"), "{refused}");
    let again = tool_result(&recorded[5], "toolu_again");
    assert!(again.contains("has since finished: it raised"), "{again}");
    assert!(again.contains("CancelledError"), "{again}");
    assert!(again.contains("[this call]\n2\n"), "{again}");
}

/// A round dropped while its code blocks in a call -- `time.sleep` here --
/// leaves that code holding the interpreter, and a cancel cannot reach a call
/// that never yields. The stop interrupts it, and the next round runs.
#[tokio::test]
async fn a_stop_breaks_the_blocking_call_a_dropped_round_left_running() {
    let running = Running::new();
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_STOP_BLOCKED",
        MODEL,
        "max-tokens = 4096",
        vec![
            submit("toolu_sleep", &running.then("import time\ntime.sleep(60)")),
            submit("toolu_next", "2"),
            text_reply("ran"),
        ],
    )
    .await;

    dropped_round(&mut agent, "sleep", running.reached()).await;
    let said = agent
        .stop_held()
        .expect("an execution was left holding the interpreter");
    assert!(said.starts_with("stopping execution"), "{said}");
    slot_freed(&agent.interpreter).await;

    assert_eq!(round(&mut agent, "again").await, "ran");
    let recorded = mock_http::drain(&mut requests);
    let next = tool_result(&recorded[2], "toolu_next");
    assert!(next.contains("has since finished: it raised"), "{next}");
    assert!(next.contains("KeyboardInterrupt"), "{next}");
    assert!(next.contains("[this call]\n2\n"), "{next}");
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::container::ExecOptions;
    use crate::python::testing::{ALPINE, pull};
    use crate::{LaunchSpec, Outrig};

    /// The public entry end to end: `start` finds the payload in a launched
    /// session's primary and starts the interpreter there, and what the
    /// model's source reads is the container's filesystem, not the host's.
    /// Recorded, the session's log directory holds what it did, under the
    /// session its containers are named for, readable by its owner alone.
    #[tokio::test]
    async fn a_round_runs_its_source_in_the_session_container() {
        pull(ALPINE).await;
        let session = tempfile::tempdir().expect("a session dir");
        let outrig = Outrig::launch(&LaunchSpec::from_image(
            ALPINE,
            BTreeMap::new(),
            session.path().join("logs"),
        ))
        .await
        .unwrap_or_else(|e| panic!("{e}"));

        let (addr, mut requests) = mock_http::start(vec![
            submit(
                "toolu_e2e",
                "print(open('/etc/alpine-release').read(), end='')",
            ),
            text_reply("read it"),
        ])
        .await;
        let var = "OUTRIG_TEST_AGENT_E2E";
        // The model row's keys end the row, so a table after them is the
        // config's own.
        let cfg = config_in(
            Style::Anthropic,
            addr,
            var,
            MODEL,
            "[events]\nmode = \"record\"",
            "max-tokens = 4096",
        );
        unsafe { std::env::set_var(var, KEY) };
        let started = within(PythonAgent::start(&outrig, &cfg, Some("coding"), None)).await;
        unsafe { std::env::remove_var(var) };
        let mut agent = started.unwrap_or_else(|e| panic!("{e}"));
        assert!(
            agent.container_name().starts_with("outrig-"),
            "{}",
            agent.container_name()
        );

        assert_eq!(round(&mut agent, "which alpine?").await, "read it");

        let release = outrig
            .exec_capture(
                &["cat".to_string(), "/etc/alpine-release".to_string()],
                &ExecOptions::new(),
            )
            .await
            .expect("cat runs");
        let recorded = mock_http::drain(&mut requests);
        assert_eq!(
            tool_result(&recorded[1], "toolu_e2e"),
            String::from_utf8_lossy(&release.stdout)
        );

        let container = agent.container_name().to_string();
        let suffix = outrig.primary().session_suffix().to_string();
        within(agent.shutdown())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        outrig.shutdown().await.expect("the session shuts down");

        let logs = session.path().join("logs");
        let records = events::recorded(&logs);
        assert_eq!(records[0]["source"], format!("/outrig/session/{suffix}"));
        assert_eq!(
            events::of_kind(&records, "agent.started")[0]["container"],
            json!(container)
        );
        assert!(!events::of_kind(&records, "exec.submitted").is_empty());
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(logs.join(crate::events::EVENTS_LOG))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
