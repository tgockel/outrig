use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use rig::completion::Message;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;

const SOURCE: &str = "/outrig/session/20260921T103000-a1b2";

fn execid(n: u64) -> ExecId {
    serde_json::from_value(json!(n)).expect("an id")
}

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect()
}

async fn opened_as(dir: &Path) -> Events {
    Events::open(dir, SOURCE.to_string())
        .await
        .expect("open the log")
}

/// The CloudEvents envelope, exactly: the top level holds the standard context
/// attributes and nothing else, since an extension attribute must be lower-case
/// letters and digits and nothing OutRig-specific belongs there anyway. The
/// shape `network.rs`'s Zeek test asserts, for this log's borrowed schema.
#[tokio::test]
async fn every_record_is_a_cloudevent_with_only_standard_attributes_at_the_top() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = opened_as(dir.path()).await;
    events.emit(Event::ExecSubmitted {
        execid: execid(7),
        source: "print('hi')",
    });
    events.emit(Event::OutputUnattributed { text: "stray" });
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    assert_eq!(records.len(), 2);
    let agent = &records[0];
    assert_eq!(
        keys(agent),
        BTreeSet::from([
            "specversion",
            "id",
            "source",
            "type",
            "subject",
            "time",
            "datacontenttype",
            "data",
        ])
    );
    for key in keys(agent) {
        assert!(
            key.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{key} is not a legal CloudEvents attribute name"
        );
    }
    assert_eq!(agent["specversion"], "1.0");
    assert_eq!(agent["id"], "1", "a decimal sequence, from 1");
    assert_eq!(agent["source"], SOURCE);
    assert_eq!(agent["type"], "org.outrig.exec.submitted");
    assert_eq!(agent["subject"], "agent/primary");
    assert_eq!(agent["datacontenttype"], "application/json");
    assert_eq!(agent["data"], json!({"execid": 7, "source": "print('hi')"}));
    let time = agent["time"].as_str().expect("a string");
    assert_eq!(time.len(), "2026-09-21T10:30:07.412Z".len(), "{time}");
    assert!(
        time.ends_with('Z') && time.as_bytes()[19] == b'.',
        "RFC 3339 in UTC, to the millisecond: {time}"
    );
    time.parse::<jiff::Timestamp>()
        .unwrap_or_else(|e| panic!("{time}: {e}"));
    assert!(
        agent.get("outrig.session_id").is_none() && agent.get("outrigsessionid").is_none(),
        "nothing of OutRig's at the top level"
    );

    let shared = &records[1];
    assert_eq!(shared["id"], "2");
    assert_eq!(shared["type"], "org.outrig.output.unattributed");
    assert!(
        shared.get("subject").is_none(),
        "output no agent can be billed for names no agent"
    );
}

/// Each event's `data`, field for field: the schema `doc/reference/events.md`
/// documents, held here so a field renamed in code is a failure rather than a
/// silent change to every reader's input.
#[test]
fn each_event_has_exactly_its_data_fields() {
    let opening = Message::user("1 message is waiting");
    let messages = [opening.clone()];
    let tools = [ToolDefinition {
        name: "submit_python".to_string(),
        description: "d".to_string(),
        parameters: json!({"type": "object"}),
    }];
    let background = [];
    let usage = Usage {
        input_tokens: 12,
        output_tokens: 7,
        total_tokens: 19,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let calls = || vec![CallUsage { index: 0, usage }];
    let id = execid(3);
    let cases: Vec<(Event<'_>, &str, &[&str])> = vec![
        (
            Event::ModelInstructions {
                model: "sonnet",
                preamble: "p",
                tools: &tools,
                max_tokens: Some(4096),
            },
            "model.instructions",
            &["model", "preamble", "tools", "max_tokens"],
        ),
        (
            Event::TurnCommitted {
                turn: 0,
                round: 1,
                incomplete: false,
                messages: &messages,
            },
            "turn.committed",
            &["turn", "round", "incomplete", "messages"],
        ),
        (
            Event::ModelCall(ModelCall {
                call: 0,
                round: 1,
                budget: CallBudget {
                    model: "sonnet",
                    window: 128_000,
                    window_assumed: true,
                    reserve: 32_000,
                    overhead: 900,
                },
                estimate: 1_000,
                carried: vec![Chosen {
                    turn: 0,
                    why: "first",
                }],
                evicted: Vec::new(),
                opening: Some(&opening),
                adjacent: vec![Repeat {
                    turn: None,
                    role: "user",
                }],
            }),
            "model.call",
            &[
                "call", "round", "budget", "estimate", "carried", "evicted", "opening", "adjacent",
            ],
        ),
        (
            Event::ExecSubmitted {
                execid: id,
                source: "x",
            },
            "exec.submitted",
            &["execid", "source"],
        ),
        (
            Event::AgentStarted {
                model: "sonnet",
                python: "3.13.15",
                container: "outrig-x",
                tool_call_max: 50,
                tool_result_max: 262_144,
            },
            "agent.started",
            &[
                "model",
                "python",
                "container",
                "tool_call_max",
                "tool_result_max",
            ],
        ),
        (Event::AgentStopped {}, "agent.stopped", &[]),
        (
            Event::RoundStarted { round: 1 },
            "round.started",
            &["round"],
        ),
        (
            Event::ExecRefused {
                execid: id,
                holder: execid(2),
            },
            "exec.refused",
            &["execid", "holder"],
        ),
        (
            Event::ExecCompleted {
                execid: id,
                status: "ok",
                duration: 0.5,
                output: "hi\n",
                dropped: 0,
                error: None,
                background: &background,
            },
            "exec.completed",
            &[
                "execid",
                "status",
                "duration",
                "output",
                "dropped",
                "error",
                "background",
            ],
        ),
        (
            Event::MemoryExhausted { execid: id },
            "memory.exhausted",
            &["execid"],
        ),
        (
            Event::ExecCancelSent { execid: id },
            "exec.cancel.sent",
            &["execid"],
        ),
        (
            Event::ExecInterruptSent {
                execid: id,
                runaway: true,
            },
            "exec.interrupt.sent",
            &["execid", "runaway"],
        ),
        (
            Event::ExecProbeFailed {
                execid: id,
                verdict: "spinning",
            },
            "exec.probe.failed",
            &["execid", "verdict"],
        ),
        (
            Event::ExecAbandoned {
                execid: id,
                why: "runaway",
            },
            "exec.abandoned",
            &["execid", "why"],
        ),
        (
            Event::InventoryObserved {
                execid: id,
                names: vec![Held {
                    name: "x",
                    kind: "int",
                }],
                total: 1,
                more: 0,
            },
            "inventory.observed",
            &["execid", "names", "total", "more"],
        ),
        (
            Event::ToolResultTruncated {
                execid: id,
                size: 2_000,
                max: 1_024,
                kept: 600,
            },
            "tool.result.truncated",
            &["execid", "size", "max", "kept"],
        ),
        (
            Event::ContextPromoted { turns: &[1, 2] },
            "context.promoted",
            &["turns"],
        ),
        (
            Event::ContextDemoted { turns: &[1] },
            "context.demoted",
            &["turns"],
        ),
        (
            Event::OutputUnattributed { text: "t" },
            "output.unattributed",
            &["text"],
        ),
        (
            Event::InterpreterDiagnostic { text: "t" },
            "interpreter.diagnostic",
            &["text"],
        ),
        (
            Event::InterpreterExited { cause: "c" },
            "interpreter.exited",
            &["cause"],
        ),
        (
            Event::ModelRoundCompleted {
                round: 1,
                stopped: None,
                usage,
                calls: calls(),
                input_tokens_max: 12,
            },
            "model.round.completed",
            &["round", "stopped", "usage", "calls", "input_tokens_max"],
        ),
        (
            Event::ModelRoundFailed {
                round: 1,
                error: "e",
                calls: calls(),
            },
            "model.round.failed",
            &["round", "error", "calls"],
        ),
        (
            Event::ModelRoundDropped {
                round: 1,
                calls: calls(),
            },
            "model.round.dropped",
            &["round", "calls"],
        ),
        (
            Event::ModelRetry {
                attempt: 2,
                delay: 1.5,
                error: "429",
            },
            "model.retry",
            &["attempt", "delay", "error"],
        ),
        (
            Event::ModelFailover {
                from: "a",
                to: "b",
                error: "e",
            },
            "model.failover",
            &["from", "to", "error"],
        ),
        (
            Event::MessageSent {
                message: id,
                channel: "user",
                from: "user",
                to: "agent/primary",
                body: "hi",
            },
            "message.sent",
            &["message", "channel", "from", "to", "body"],
        ),
        (
            Event::MessageRefused {
                message: id,
                channel: "user",
                from: "user",
                to: "agent/primary",
                reason: "full",
            },
            "message.refused",
            &["message", "channel", "from", "to", "reason"],
        ),
        (
            Event::MessageReceived {
                message: id,
                channel: "user",
                from: "user",
                to: "agent/primary",
            },
            "message.received",
            &["message", "channel", "from", "to"],
        ),
    ];

    let mut seen = BTreeSet::new();
    for (event, kind, fields) in &cases {
        assert_eq!(event.kind(), *kind);
        assert!(seen.insert(*kind), "{kind} twice");
        let data = serde_json::to_value(event).expect("encodes");
        assert_eq!(
            keys(&data),
            fields.iter().copied().collect::<BTreeSet<_>>(),
            "{kind}"
        );
    }

    let nested = |event: &Event<'_>, field: &str| {
        let data = serde_json::to_value(event).expect("encodes");
        keys(&data[field])
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let completed = &cases
        .iter()
        .find(|(_, kind, _)| *kind == "model.round.completed")
        .expect("the round")
        .0;
    assert_eq!(
        nested(completed, "usage"),
        [
            "cache_creation_input_tokens",
            "cached_input_tokens",
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
        ]
    );
    let call = &cases
        .iter()
        .find(|(_, kind, _)| *kind == "model.call")
        .expect("the call")
        .0;
    assert_eq!(
        nested(call, "budget"),
        ["model", "overhead", "reserve", "window", "window_assumed"]
    );
    let data = serde_json::to_value(call).expect("encodes");
    assert_eq!(data["carried"], json!([{"turn": 0, "why": "first"}]));
    assert_eq!(data["adjacent"], json!([{"turn": null, "role": "user"}]));
    // rig's own form of the message. Compared as JSON: read back, rig fills in
    // an empty `additional_params` the original did not have.
    assert_eq!(
        data["opening"],
        serde_json::to_value(&opening).expect("rig encodes it")
    );
    serde_json::from_value::<Message>(data["opening"].clone()).expect("and reads it back");
}

/// The id a record is given is its place in the file, whichever task emitted
/// it: numbering and buffering are one step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ids_are_the_files_order_whoever_emits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = opened(dir.path()).await;
    let tasks: Vec<_> = (0..8)
        .map(|task| {
            let events = events.clone();
            tokio::spawn(async move {
                for n in 0..200 {
                    events.ready().await;
                    events.emit(Event::InterpreterDiagnostic {
                        text: &format!("{task}-{n}"),
                    });
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.expect("emitted");
    }
    events.close().await.expect("nothing lost");

    let ids: Vec<u64> = recorded(dir.path())
        .iter()
        .map(|record| {
            record["id"]
                .as_str()
                .expect("a string")
                .parse()
                .expect("decimal")
        })
        .collect();
    assert_eq!(ids, (1..=1600).collect::<Vec<_>>());
}

/// A log that already holds a recording is refused, and kept: a second
/// recording in it would repeat the first's `source` and ids, and a reader
/// that deduplicates by them would drop it. Empty, it is taken.
#[tokio::test]
async fn a_log_that_holds_a_recording_is_refused_and_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let first = opened(dir.path()).await;
    first.emit(Event::AgentStopped {});
    first.close().await.expect("nothing lost");
    released(dir.path()).await;
    let path = dir.path().join(EVENTS_LOG);
    let before = std::fs::read(&path).expect("the first recording");

    let refused = Events::open(dir.path(), TEST_SOURCE.to_string())
        .await
        .err()
        .expect("a second recording is refused")
        .to_string();
    assert!(refused.contains("already holds a recording"), "{refused}");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "and the first is kept"
    );

    let empty = tempfile::tempdir().expect("tempdir");
    std::fs::write(empty.path().join(EVENTS_LOG), b"").expect("an empty log");
    opened(empty.path())
        .await
        .close()
        .await
        .expect("an empty log is taken");
}

/// A log nobody should read but its owner is made that way, whatever the umask,
/// and whatever mode a file already there had.
#[tokio::test]
async fn the_log_is_its_owners_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = opened(dir.path()).await;
    events.close().await.expect("nothing lost");
    let mode = |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&dir.path().join(EVENTS_LOG)), 0o600);

    let other = tempfile::tempdir().expect("tempdir");
    let path = other.path().join(EVENTS_LOG);
    std::fs::write(&path, b"").expect("create");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    opened(other.path())
        .await
        .close()
        .await
        .expect("nothing lost");
    assert_eq!(mode(&path), 0o600, "an earlier file is narrowed");
}

/// Recording does not change what it records, so nothing that emits waits,
/// and what a full buffer cannot take is counted rather than lost quietly. A
/// caller that can wait, waits: a stalled writer holds it up. And a close is
/// bounded by its deadline, reporting -- as agent events, not network ones --
/// everything the file did not get.
#[tokio::test(start_paused = true)]
async fn a_stalled_writer_holds_up_who_waits_and_close_counts_what_it_lost() {
    // A writer that never takes anything off its queue.
    let (records, _queue) = mpsc::channel(QUEUE);
    let sink = LineSink::queuing_to(
        records,
        Some(tokio::spawn(std::future::pending())),
        &EVENT_LABELS,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let events = Events::over(sink, dir.path().join(EVENTS_LOG), TEST_SOURCE.to_string());

    let emitted = QUEUE + 10;
    for n in 0..emitted {
        events.emit(Event::InterpreterDiagnostic {
            text: &n.to_string(),
        });
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(60), events.ready())
            .await
            .is_err(),
        "a caller that can wait waits for room that is not coming"
    );
    // Emitting still never waits.
    events.emit(Event::AgentStopped {});

    let started = tokio::time::Instant::now();
    let lost = events.close().await.expect_err("nothing was written");
    assert_eq!(
        started.elapsed(),
        line_sink::SHUTDOWN_GRACE,
        "bounded by its deadline"
    );
    assert_eq!(
        lost.records,
        emitted as u64 + 1,
        "every one: those the writer held, and those there was no room for"
    );
    assert!(
        lost.integrity.is_some(),
        "a writer stopped mid-write: {lost}"
    );
    assert!(lost.first.contains("more than"), "the first lost: {lost}");
    let text = lost.to_string();
    assert!(
        text.starts_with(&format!("{} agent event(s)", emitted + 1)),
        "{text}"
    );
    assert!(!text.contains("network"), "{text}");

    // Closed means closed: nothing after it is recorded, or counted.
    events.emit(Event::AgentStopped {});
    assert!(
        events.close().await.is_ok(),
        "and a second close has nothing to say"
    );
}

/// The last handle dropped without a close still finishes the file, in the
/// background.
#[tokio::test]
async fn a_log_dropped_without_a_close_is_finished_anyway() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = opened(dir.path()).await;
    events.emit(Event::AgentStopped {});
    drop(events);
    let path = dir.path().join(EVENTS_LOG);
    let deadline = tokio::time::Instant::now() + line_sink::SHUTDOWN_GRACE;
    while std::fs::read_to_string(&path).map_or(true, |text| text.is_empty()) {
        assert!(tokio::time::Instant::now() < deadline, "never written");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(kinds(&recorded(dir.path())), ["agent.stopped"]);
}

/// Off is nothing at all: no file, and nothing to wait for.
#[tokio::test]
async fn off_records_nothing_and_never_waits() {
    let events = Events::off();
    assert!(!events.is_on());
    events.emit(Event::AgentStopped {});
    events.ready().await;
    events.close().await.expect("nothing to lose");
}
