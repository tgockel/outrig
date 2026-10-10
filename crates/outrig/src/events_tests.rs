use crate::config::RoleAlternation;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use rig::completion::Message;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;
use crate::agent::rig_json;
use crate::harness::event::*;
use crate::harness::{ClosedBy, ExecutionOutcome, ExecutionStatus, Stopped, Verdict};

const SOURCE: &str = "/outrig/session/20260921T103000-a1b2";

fn execid(n: u64) -> ExecId {
    ExecId::new(n)
}

fn diagnostic(text: impl ToString) -> Payload {
    Payload::InterpreterDiagnostic {
        text: text.to_string(),
    }
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
    let mut stream = StreamBuilder::default();
    stream
        .record(dir, SOURCE.to_string())
        .await
        .expect("open the log");
    stream.build()
}

/// The CloudEvents envelope, exactly: the top level holds the standard context
/// attributes and nothing else, since an extension attribute must be lower-case
/// letters and digits and nothing OutRig-specific belongs there anyway. The
/// shape `network.rs`'s Zeek test asserts, for this log's borrowed schema.
#[tokio::test]
async fn every_record_is_a_cloudevent_with_only_standard_attributes_at_the_top() {
    let dir = tempfile::tempdir().expect("tempdir");
    let events = opened_as(dir.path()).await;
    events.emit(Payload::ExecSubmitted {
        execid: execid(7),
        source: "print('hi')".to_string(),
    });
    events.emit(Payload::OutputUnattributed {
        text: "stray".to_string(),
    });
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
    let tools = vec![ToolDefinition {
        name: "submit_python".to_string(),
        description: "d".to_string(),
        parameters: json!({"type": "object"}),
    }];
    let usage = Usage {
        input_tokens: 12,
        output_tokens: 7,
        total_tokens: 19,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        reasoning_tokens: 0,
    };
    let (call, attempt) = (CallId::new(1), AttemptId::new(2));
    let calls = || {
        vec![CallUsage {
            index: 0,
            model: "sonnet".into(),
            call_id: call,
            attempt_id: attempt,
            usage: Some(usage),
        }]
    };
    let id = execid(3);
    let text = |s: &str| s.to_string();
    let cases: Vec<(Payload, &str, &[&str])> = vec![
        (
            Payload::ModelInstructions {
                model: text("sonnet"),
                preamble: text("p"),
                tools: tools.clone(),
                max_tokens: Some(4096),
            },
            "model.instructions",
            &["model", "preamble", "tools", "max_tokens"],
        ),
        (
            Payload::TurnCommitted {
                turn: 0,
                round: 1,
                incomplete: false,
                messages: vec![rig_json(&opening)],
            },
            "turn.committed",
            &["turn", "round", "incomplete", "messages"],
        ),
        (
            Payload::ModelCall(ModelCall {
                call: 0,
                call_id: call,
                round: 1,
                budget: CallBudget {
                    model: text("sonnet"),
                    window: 128_000,
                    window_assumed: true,
                    reserve: 32_000,
                    overhead: 900,
                    max_tokens: Some(4096),
                    role_alternation: RoleAlternation::Relaxed,
                },
                estimate: 1_000,
                carried: vec![Chosen {
                    turn: 0,
                    why: Why::First,
                }],
                evicted: Vec::new(),
                withheld: vec![Chosen {
                    turn: 2,
                    why: Why::Promoted,
                }],
                opening: Some(rig_json(&opening)),
                adjacent: vec![Repeat {
                    turn: None,
                    role: Role::User,
                }],
                left_out: vec![LeftOut {
                    turn: 0,
                    message: 1,
                    part: 0,
                }],
            }),
            "model.call",
            &[
                "call", "call_id", "round", "budget", "estimate", "carried", "evicted", "withheld",
                "opening", "adjacent", "left_out",
            ],
        ),
        (
            Payload::ExecSubmitted {
                execid: id,
                source: text("x"),
            },
            "exec.submitted",
            &["execid", "source"],
        ),
        (
            Payload::SessionState {
                state: SessionState::RoundRunning,
            },
            "session.state",
            &["state"],
        ),
        (
            Payload::SessionReport {
                closed_by: ClosedBy::Owner,
                stopped: Stopped::Proven,
                executions: vec![ExecutionOutcome {
                    id,
                    status: ExecutionStatus::Unknown,
                }],
                verdict: Verdict::StoppedWithUnknown,
            },
            "session.report",
            &["closed_by", "stopped", "executions", "verdict"],
        ),
        (
            Payload::AgentStarted {
                model: text("sonnet"),
                python: text("3.13.15"),
                container: text("outrig-x"),
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
        (Payload::AgentStopped {}, "agent.stopped", &[]),
        (
            Payload::RoundStarted { round: 1 },
            "round.started",
            &["round"],
        ),
        (
            Payload::ExecRefused {
                execid: id,
                holder: Some(execid(2)),
                reason: Refusal::Held,
            },
            "exec.refused",
            &["execid", "holder", "reason"],
        ),
        (
            Payload::ExecCompleted {
                execid: id,
                status: ExecStatus::Ok,
                duration: Duration::from_millis(500),
                output: text("hi\n"),
                dropped: 0,
                error: None,
                background: Vec::new(),
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
            Payload::MemoryExhausted { execid: id },
            "memory.exhausted",
            &["execid"],
        ),
        (
            Payload::ExecCancelSent { execid: id },
            "exec.cancel.sent",
            &["execid"],
        ),
        (
            Payload::ExecInterruptSent {
                execid: id,
                runaway: true,
            },
            "exec.interrupt.sent",
            &["execid", "runaway"],
        ),
        (
            Payload::ExecProbeFailed {
                execid: id,
                verdict: ProbeVerdict::Spinning,
            },
            "exec.probe.failed",
            &["execid", "verdict"],
        ),
        (
            Payload::ExecAbandoned {
                execid: id,
                why: Abandoned::Runaway,
            },
            "exec.abandoned",
            &["execid", "why"],
        ),
        (
            Payload::InventoryObserved {
                execid: id,
                names: vec![Held {
                    name: text("x"),
                    type_name: text("int"),
                }],
                total: 1,
                more: 0,
            },
            "inventory.observed",
            &["execid", "names", "total", "more"],
        ),
        (
            Payload::ToolResultTruncated {
                execid: id,
                size: 2_000,
                max: 1_024,
                kept: 600,
            },
            "tool.result.truncated",
            &["execid", "size", "max", "kept"],
        ),
        (
            Payload::ContextPromoted { turns: vec![1, 2] },
            "context.promoted",
            &["turns"],
        ),
        (
            Payload::ContextDemoted { turns: vec![1] },
            "context.demoted",
            &["turns"],
        ),
        (
            Payload::OutputUnattributed { text: text("t") },
            "output.unattributed",
            &["text"],
        ),
        (
            Payload::InterpreterDiagnostic { text: text("t") },
            "interpreter.diagnostic",
            &["text"],
        ),
        (
            Payload::InterpreterExited {
                cause: text("c"),
                expected: false,
            },
            "interpreter.exited",
            &["cause", "expected"],
        ),
        (
            Payload::ModelRoundCompleted {
                round: 1,
                stopped: None,
                usage: Some(usage),
                calls: calls(),
                attempts: vec![attempt],
                input_tokens_max: Some(12),
            },
            "model.round.completed",
            &[
                "round",
                "stopped",
                "usage",
                "calls",
                "attempts",
                "input_tokens_max",
            ],
        ),
        (
            Payload::ModelRoundFailed {
                round: 1,
                error: text("e"),
                calls: calls(),
                usage: None,
                attempts: vec![attempt],
            },
            "model.round.failed",
            &["round", "error", "calls", "usage", "attempts"],
        ),
        (
            Payload::ModelRoundDropped {
                round: 1,
                calls: calls(),
                usage: None,
                attempts: vec![attempt],
            },
            "model.round.dropped",
            &["round", "calls", "usage", "attempts"],
        ),
        (
            Payload::ModelAttempt(ModelAttempt {
                call_id: call,
                attempt_id: attempt,
                model: text("sonnet"),
                identifier: text("claude-sonnet-4-6"),
                max_tokens: Some(4096),
                error: Some(text("HTTP 429 Too Many Requests")),
                usage: None,
            }),
            "model.attempt",
            &[
                "call_id",
                "attempt_id",
                "model",
                "identifier",
                "max_tokens",
                "error",
                "usage",
            ],
        ),
        (
            Payload::ModelRetry {
                model: text("sonnet"),
                attempt: 2,
                delay: Duration::from_millis(1_500),
                error: text("429"),
                call_id: call,
                attempt_id: attempt,
            },
            "model.retry",
            &[
                "model",
                "attempt",
                "delay",
                "error",
                "call_id",
                "attempt_id",
            ],
        ),
        (
            Payload::ModelFailover {
                from: text("a"),
                to: text("b"),
                error: text("e"),
                call_id: call,
                attempt_id: Some(attempt),
            },
            "model.failover",
            &["from", "to", "error", "call_id", "attempt_id"],
        ),
        (
            Payload::ModelUsageReplaced {
                call_id: call,
                attempt_id: attempt,
                usage,
            },
            "model.usage.replaced",
            &["call_id", "attempt_id", "usage"],
        ),
        (
            Payload::ModelUsageRefused {
                call_id: Some(call),
                attempt_id: attempt,
                usage,
                reason: UsageRefusal::Replaced,
            },
            "model.usage.refused",
            &["call_id", "attempt_id", "usage", "reason"],
        ),
        (
            Payload::MessageSent {
                message: MessageId::new(3),
                channel: text("user"),
                from: text("user"),
                to: text("agent/primary"),
                body: text("hi"),
            },
            "message.sent",
            &["message", "channel", "from", "to", "body"],
        ),
        (
            Payload::MessageRefused {
                message: MessageId::new(3),
                channel: text("user"),
                from: text("user"),
                to: text("agent/primary"),
                reason: text("full"),
            },
            "message.refused",
            &["message", "channel", "from", "to", "reason"],
        ),
        (
            Payload::MessageReceived {
                message: MessageId::new(3),
                channel: text("user"),
                from: text("user"),
                to: text("agent/primary"),
            },
            "message.received",
            &["message", "channel", "from", "to"],
        ),
    ];

    let mut seen = BTreeSet::new();
    for (payload, kind, fields) in &cases {
        assert_eq!(payload.kind(), *kind);
        assert!(seen.insert(*kind), "{kind} twice");
        let data = serde_json::to_value(payload).expect("encodes");
        assert_eq!(
            keys(&data),
            fields.iter().copied().collect::<BTreeSet<_>>(),
            "{kind}"
        );
    }

    let nested = |payload: &Payload, field: &str| {
        let data = serde_json::to_value(payload).expect("encodes");
        keys(&data[field])
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let case = |kind: &str| {
        &cases
            .iter()
            .find(|(_, case, _)| *case == kind)
            .expect("the case")
            .0
    };
    let completed = case("model.round.completed");
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
    let data = serde_json::to_value(completed).expect("encodes");
    assert_eq!(
        keys(&data["calls"][0]),
        BTreeSet::from(["index", "model", "call_id", "attempt_id", "usage"]),
        "each call names the model and the attempt that answered it"
    );
    let failed = serde_json::to_value(case("model.round.failed")).expect("encodes");
    assert_eq!(failed["usage"], Value::Null, "null, never zero");
    let call = case("model.call");
    assert_eq!(
        nested(call, "budget"),
        [
            "max_tokens",
            "model",
            "overhead",
            "reserve",
            "role_alternation",
            "window",
            "window_assumed"
        ]
    );
    let data = serde_json::to_value(call).expect("encodes");
    assert_eq!(data["carried"], json!([{"turn": 0, "why": "first"}]));
    assert_eq!(data["withheld"], json!([{"turn": 2, "why": "promoted"}]));
    assert_eq!(data["adjacent"], json!([{"turn": null, "role": "user"}]));
    assert_eq!(
        data["left_out"],
        json!([{"turn": 0, "message": 1, "part": 0}])
    );
    // rig's own form of the message. Compared as JSON: read back, rig fills in
    // an empty `additional_params` the original did not have.
    assert_eq!(
        data["opening"],
        serde_json::to_value(&opening).expect("rig encodes it")
    );
    serde_json::from_value::<Message>(data["opening"].clone()).expect("and reads it back");
    // OutRig's tool definition is written as rig's was.
    assert_eq!(
        serde_json::to_value(&tools[0]).expect("encodes"),
        serde_json::to_value(rig::completion::ToolDefinition {
            name: "submit_python".to_string(),
            description: "d".to_string(),
            parameters: json!({"type": "object"}),
        })
        .expect("rig encodes it")
    );
    let completed = serde_json::to_value(case("exec.completed")).expect("encodes");
    assert_eq!(completed["duration"], json!(0.5), "seconds, as a number");
    let report = serde_json::to_value(case("session.report")).expect("encodes");
    assert_eq!(
        report,
        json!({
            "closed_by": {"by": "owner"},
            "stopped": {"state": "proven"},
            "executions": [{"execid": 3, "status": "unknown"}],
            "verdict": "stopped_with_unknown",
        })
    );
    let state = serde_json::to_value(case("session.state")).expect("encodes");
    assert_eq!(state, json!({"state": "round_running"}));
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
                    events.emit(diagnostic(format!("{task}-{n}")));
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
    first.emit(Payload::AgentStopped {});
    first.close().await.expect("nothing lost");
    released(dir.path()).await;
    let path = dir.path().join(EVENTS_LOG);
    let before = std::fs::read(&path).expect("the first recording");

    let refused = StreamBuilder::default()
        .record(dir.path(), TEST_SOURCE.to_string())
        .await
        .expect_err("a second recording is refused")
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

/// Recording does not change what it records, so nothing that emits waits:
/// the log's backpressure stops at its own subscription. A writer whose file
/// has stopped taking anything leaves events waiting there, and those that
/// overflow it are a counted gap -- the session never waits. A close is
/// bounded by its deadline, reporting -- as agent events, not network ones --
/// everything the file did not get.
#[tokio::test(start_paused = true)]
async fn a_stalled_writer_loses_a_counted_gap_and_close_counts_what_it_lost() {
    // A writer that never takes anything off its queue.
    let (records, queue) = mpsc::channel(line_sink::QUEUE);
    let sink = LineSink::queuing_to(
        records,
        Some(tokio::spawn(std::future::pending())),
        &EVENT_LABELS,
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let mut stream = StreamBuilder::default();
    stream.record_over(sink, dir.path().join(EVENTS_LOG), TEST_SOURCE.to_string());
    let events = stream.build();

    // The writer takes events until its file holds all it has room for.
    for n in 0..line_sink::QUEUE {
        events.emit(diagnostic(n));
    }
    while queue.len() < line_sink::QUEUE {
        tokio::task::yield_now().await;
    }
    // Then its subscription fills, and overflows.
    let over = 10;
    let started = tokio::time::Instant::now();
    for n in 0..DEFAULT_CAPACITY + over {
        events.emit(diagnostic(n));
    }
    // Emitting still never waits.
    events.emit(Payload::AgentStopped {});
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "nothing emitting waited for the writer"
    );
    let emitted = line_sink::QUEUE + DEFAULT_CAPACITY + over + 1;

    let started = tokio::time::Instant::now();
    let lost = events.close().await.expect_err("nothing was written");
    assert_eq!(
        started.elapsed(),
        line_sink::SHUTDOWN_GRACE,
        "bounded by its deadline"
    );
    assert_eq!(
        lost.records, emitted as u64,
        "every one: those the sink held, those its subscription held, and the gap"
    );
    assert!(
        lost.integrity.is_some(),
        "a writer stopped mid-write: {lost}"
    );
    assert!(lost.first.contains("behind"), "the first lost: {lost}");
    let text = lost.to_string();
    assert!(
        text.starts_with(&format!("{emitted} agent event(s)")),
        "{text}"
    );
    assert!(!text.contains("network"), "{text}");

    // Closed means closed: nothing after it is recorded, or counted.
    events.emit(Payload::AgentStopped {});
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
    events.emit(Payload::AgentStopped {});
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
    events.emit(Payload::AgentStopped {});
    events.close().await.expect("nothing to lose");
    assert_eq!(events.tally(), Tally::default());
}

/// Every subscriber sees every event, in id order, whichever task published
/// it, and the log beside them holds the same ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_subscriber_sees_every_event_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut stream = StreamBuilder::default();
    let mut first = stream.subscribe(DEFAULT_CAPACITY);
    let mut second = stream.subscribe(DEFAULT_CAPACITY);
    stream
        .record(dir.path(), TEST_SOURCE.to_string())
        .await
        .expect("open the log");
    let events = stream.build();
    let tasks: Vec<_> = (0..8)
        .map(|task| {
            let events = events.clone();
            tokio::spawn(async move {
                for n in 0..200 {
                    events.emit(diagnostic(format!("{task}-{n}")));
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.expect("emitted");
    }
    events.close().await.expect("nothing lost");
    for subscription in [&mut first, &mut second] {
        let mut ids = Vec::new();
        while let Some(received) = subscription.recv().await {
            match received {
                Received::Event(event) => ids.push(event.id),
                Received::Missed(n) => panic!("missed {n}"),
            }
        }
        assert_eq!(ids, (1..=1600).collect::<Vec<_>>());
    }
    let logged: Vec<u64> = recorded(dir.path())
        .iter()
        .map(|record| {
            record["id"]
                .as_str()
                .expect("an id")
                .parse()
                .expect("decimal")
        })
        .collect();
    assert_eq!(logged, (1..=1600).collect::<Vec<_>>());
    assert_eq!(
        events.tally(),
        Tally {
            last: 1600,
            missed: vec![0, 0]
        }
    );
}

/// A subscriber that falls behind loses its oldest events, never slowing the
/// stream, and is told how many before the next one it gets -- whose id is
/// that many past the last it saw. What it lost is counted whether it reads or
/// not.
#[tokio::test]
async fn a_subscriber_that_falls_behind_is_told_what_it_missed() {
    let mut stream = StreamBuilder::default();
    let mut slow = stream.subscribe(4);
    let silent = stream.subscribe(2);
    let events = stream.build();
    for n in 0..10 {
        events.emit(diagnostic(n));
    }
    assert!(matches!(slow.try_recv(), Ok(Received::Missed(6))));
    let mut ids = Vec::new();
    while let Ok(Received::Event(event)) = slow.try_recv() {
        ids.push(event.id);
    }
    assert_eq!(ids, [7, 8, 9, 10]);
    assert!(matches!(slow.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!((slow.missed(), silent.missed()), (6, 8));
    assert_eq!(events.tally().missed, [6, 8]);

    events.close().await.expect("nothing to lose");
    assert!(matches!(slow.try_recv(), Err(TryRecvError::Closed)));
    assert!(slow.recv().await.is_none());
}

/// The event that ends the stream is the last every subscriber sees, however
/// many others are being published at the same moment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_closing_event_is_the_last_of_the_stream() {
    let mut stream = StreamBuilder::default();
    let mut subscription = stream.subscribe(100_000);
    let events = stream.build();
    let emitting = {
        let events = events.clone();
        tokio::spawn(async move {
            for n in 0..20_000 {
                events.emit(diagnostic(n));
            }
        })
    };
    tokio::task::yield_now().await;
    events
        .close_after(Payload::AgentStopped {})
        .await
        .expect("nothing to lose");
    emitting.await.expect("emitted");
    let mut last = None;
    while let Some(received) = subscription.recv().await {
        if let Received::Event(event) = received {
            last = Some(event.kind());
        }
    }
    assert_eq!(last, Some("agent.stopped"));
}

/// A stream given up before it was built -- a session that failed to start --
/// ends its subscriptions.
#[tokio::test]
async fn an_unbuilt_stream_ends_its_subscriptions() {
    let mut stream = StreamBuilder::default();
    let mut subscription = stream.subscribe(4);
    drop(stream);
    assert!(subscription.recv().await.is_none());
}
