//! The agent loop, driven against a scripted Anthropic endpoint and the
//! payload's real interpreter run on the host.
//!
//! What reached the model is read off the requests the mock recorded, so a
//! test asserts on the wire rather than on anything the loop reports about
//! itself. The e2e module at the end starts the interpreter through podman,
//! which is the one path the rest skips.

use std::sync::{Arc, Mutex};

use rig::tool::{ToolDyn, ToolError};
use serde_json::json;

use super::build::{ANTHROPIC_FALLBACK_MAX_TOKENS, anthropic_model};
use super::mock_http::{self, CannedResponse, MODEL, RecordedRequest, failure, submit, text_reply};
use super::resolve::{LlmResolveError, ResolvedCandidate, ResolvedProvider, resolve_agent};
use super::tool::{self, SubmitPython, render, truncate_for_llm};
use super::{AgentError, PythonAgent};
use crate::config::Config;
use crate::python::host::{Background, ExecId, Late, Outcome, Report, Unknown};
use crate::python::testing::{ok, start_on_host, within};

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
    load(&format!(
        r#"
default-model = "sonnet"

[providers.claude]
style                = "anthropic"
base-url             = "http://{addr}"
api-key              = "${{{var}}}"
request-timeout-secs = 10

[models.sonnet]
provider   = "claude"
identifier = "{identifier}"

[agents.coding]
preamble = "You write Python."
{agent_keys}
"#
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
    let (addr, requests) = mock_http::start(script).await;
    let cfg = config(addr, var, identifier, agent_keys);
    let interpreter = start_on_host().await;
    let agent = with_key(var, || {
        PythonAgent::with_interpreter(interpreter, &cfg, Some("coding"), None)
    })
    .unwrap_or_else(|e| panic!("{e}"));
    (agent, requests)
}

async fn round(agent: &mut PythonAgent, prompt: &str) -> String {
    within(agent.round(prompt))
        .await
        .unwrap_or_else(|e| panic!("the round failed: {e}"))
}

/// The system prompt `request` carried, whether rig sent it as one string or
/// as text blocks.
fn system_prompt(request: &RecordedRequest) -> String {
    match &request.body["system"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .map(|block| block["text"].as_str().expect("a text block"))
            .collect(),
        other => panic!("no system prompt: {other}"),
    }
}

/// The text of the `tool_result` for `id` in `request`, as the model reads it.
fn tool_result(request: &RecordedRequest, id: &str) -> String {
    let block = request.body["messages"]
        .as_array()
        .expect("a messages array")
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
    let second_round = recorded[2].body["messages"].as_array().expect("messages");
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

    let err = within(agent.round("set x"))
        .await
        .expect_err("the second model call failed");
    let err = err.to_string();
    assert!(
        err.contains("already run Python") && err.contains("rather than resending"),
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

    let err = within(agent.round("hello"))
        .await
        .expect_err("the model call failed")
        .to_string();
    assert!(
        err.starts_with("agent prompt failed:") && !err.contains("already run Python"),
        "{err}"
    );
    assert_eq!(round(&mut agent, "hello").await, "ok");

    let recorded = mock_http::drain(&mut requests);
    let messages = recorded[1].body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "{messages:#?}");
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
    tokio::select! {
        biased;
        () = within(second_call) => {}
        _ = agent.round("set x") => panic!("the round returned before it could be dropped"),
    }
    assert!(
        !agent.history.is_empty(),
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
    let batch = mock_http::message(
        json!([
            { "type": "tool_use", "id": "toolu_a", "name": tool::NAME,
              "input": { "source": "x = 41\nprint('a ran')" } },
            { "type": "tool_use", "id": "toolu_b", "name": tool::NAME,
              "input": { "source": "import time\ntime.sleep(30)" } },
            { "type": "tool_use", "id": "toolu_c", "name": tool::NAME,
              "input": { "source": "print('c ran')" } },
        ]),
        "tool_use",
    );
    let (mut agent, mut requests) = agent_over(
        "OUTRIG_TEST_AGENT_DROPPED_BATCH",
        MODEL,
        "max-tokens = 4096",
        vec![batch, text_reply("carried on")],
    )
    .await;
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    agent.on_submit(move |source| {
        let _ = started.send(source.to_string());
    });

    // The second source going to run means the first has returned.
    let inside_the_second = async {
        starts.recv().await.expect("the first call");
        starts.recv().await.expect("the second call");
    };
    tokio::select! {
        biased;
        () = within(inside_the_second) => {}
        _ = agent.round("run three") => panic!("the round returned before it could be dropped"),
    }

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

    tokio::select! {
        biased;
        _ = within(starts.recv()) => {}
        _ = agent.round("sleep") => panic!("the round returned before it could be dropped"),
    }
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
            && system.contains("`pip install` does not work")
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
    let text = super::orientation::preamble(Some(std::path::Path::new("/workspace")), None);
    assert!(
        text.contains("Your working directory is /workspace, which holds the project's files."),
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

// ---------------------------------------------------------------------------- the tool

/// Arguments the schema does not allow are the model's mistake to fix, so
/// they fail as arguments, and nothing is submitted.
#[tokio::test]
async fn malformed_arguments_are_a_tool_error() {
    let tool = SubmitPython::new(start_on_host().await, 1024);
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
    let (text, unreported) = render(late, outcome, max);
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
    let (text, unreported) = render(late, &ok("now\n"), 1024);
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

    let (next, rest) = render(unreported, &ok("later\n"), 1024);
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
    let tool = SubmitPython::new(start_on_host().await, 1024).with_unreported(late);
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
        },
        max_tokens,
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

/// The ceiling the agent reports is the one that reached the wire, including
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
        assert_eq!(agent.max_tokens, Some(expected), "{identifier}");
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
    assert_eq!(resolved.candidate.model_name, "head");
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
    assert_eq!(resolved.candidate.model_identifier, "gpt-tail");
    assert!(matches!(
        resolved.candidate.provider,
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

/// An alias runs against the first of its models this build can reach: one
/// whose key is unset is passed over rather than tried, and when none can be
/// reached the error names each and why.
#[test]
fn an_alias_runs_against_its_first_reachable_model() {
    let vars = "OUTRIG_TEST_AGENT_RESOLVE_ALIAS";
    let cfg = two_provider_config(vars, "");
    let both = with_both_keys(vars, || resolve_agent(&cfg, None, Some("chain"))).expect("resolves");
    assert_eq!(both.candidate.model_name, "head");

    let second = format!("{vars}_SECOND");
    let tail_only = with_key(&second, || resolve_agent(&cfg, None, Some("chain")));
    assert_eq!(tail_only.expect("resolves").candidate.model_name, "tail");

    let err = resolve_agent(&cfg, None, Some("chain")).expect_err("no key is set");
    let rendered = err.to_string();
    assert!(
        rendered.contains("no usable model for alias \"chain\"")
            && rendered.contains(&format!("{vars}_FIRST"))
            && rendered.contains(&second),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------- through podman

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::container::ExecOptions;
    use crate::python::testing::{ALPINE, pull_alpine};
    use crate::{LaunchSpec, Outrig};

    /// The public entry end to end: `start` finds the payload in a launched
    /// session's primary and starts the interpreter there, and what the
    /// model's source reads is the container's filesystem, not the host's.
    #[tokio::test]
    async fn a_round_runs_its_source_in_the_session_container() {
        pull_alpine().await;
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
        let cfg = config(addr, var, MODEL, "max-tokens = 4096");
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

        drop(agent);
        outrig.shutdown().await.expect("the session shuts down");
    }
}
