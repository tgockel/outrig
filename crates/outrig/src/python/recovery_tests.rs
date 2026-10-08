//! The waiting policy, driven two ways. Against a fake transport with time
//! paused, for exactly what the host sends given what the interpreter answers
//! -- above all, what it does *not* send. Against the real interpreter with
//! short timings, for the shapes it exists to survive.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::super::host::{ExecId, Execution, Interpreter, Outcome, PRIMARY, Unknown};
use super::super::testing::{Fake, ok, round_trip, slot_freed, start_on_host, within};
use super::{
    ATTEMPTS, GaveUp, Press, Presses, Settled, Timings, Verdict, Waited, Waiting, settle,
    stop_abandoned,
};
use crate::events::{kinds, of_kind, opened, recorded};

/// The defaults, with checks close enough together that a test waiting for
/// the next request is not failed by its own step bound first.
fn paused() -> Timings {
    Timings {
        check_every: Duration::from_secs(10),
        ..Timings::default()
    }
}

/// Short enough that the real interpreter is checked many times a second.
fn quick() -> Timings {
    Timings {
        check_every: Duration::from_millis(200),
        probe: Duration::from_millis(300),
        user_probe: Duration::from_millis(300),
        cpu: Duration::from_secs(2),
        grace: Duration::from_millis(200),
        last_look: Duration::from_millis(200),
    }
}

fn settling(
    interpreter: &Interpreter,
    execution: Execution,
    presses: &Presses,
    timings: Timings,
) -> JoinHandle<Settled> {
    let (interpreter, presses) = (interpreter.clone(), presses.clone());
    tokio::spawn(async move { settle(&interpreter, execution, &presses, &timings).await })
}

async fn cpu(fake: &mut Fake, seconds: f64) {
    let request = fake.expect("cpu").await;
    fake.send(json!({"t": "cpu", "agent": PRIMARY, "id": request["id"], "seconds": seconds}))
        .await;
}

/// A check the fake answers with the loop quiet, the thread's CPU clock going
/// from `before` to `after` across the probe. Returns the probe, which goes
/// unanswered.
async fn quiet(fake: &mut Fake, before: f64, after: f64) -> Value {
    cpu(fake, before).await;
    let probe = fake.expect("inv").await;
    cpu(fake, after).await;
    probe
}

/// The same, for a check made while an earlier probe is still unanswered:
/// that one is waited on again rather than another sent.
async fn still_quiet(fake: &mut Fake, before: f64, after: f64) {
    cpu(fake, before).await;
    cpu(fake, after).await;
}

/// A check the fake answers with the loop turning.
async fn turning(fake: &mut Fake) {
    cpu(fake, 1.0).await;
    fake.answer_inventory().await;
}

/// A check that finds an earlier, unanswered probe answered at last.
async fn turning_again(fake: &mut Fake, probe: &Value) {
    cpu(fake, 1.0).await;
    fake.inventory(&probe["id"]).await;
}

async fn submitted(interpreter: &Interpreter, fake: &mut Fake) -> Execution {
    let execution = interpreter.submit("work()").expect("submitted");
    fake.exec(execution.id()).await;
    execution
}

/// A fake interpreter, one submission to it being settled, and the guard
/// that aims `presses` at that submission.
async fn settling_one(
    presses: &Presses,
) -> (Interpreter, Fake, ExecId, JoinHandle<Settled>, Waiting) {
    let (interpreter, mut fake) = Fake::connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, presses, paused());
    (interpreter, fake, id, settled, waiting)
}

/// What the host did: the user stopped the execution.
fn user() -> Waited {
    Waited {
        user_stopped: true,
        ..Waited::default()
    }
}

/// What the host did: it interrupted a runaway.
fn runaway() -> Waited {
    Waited {
        runaway_interrupted: true,
        ..Waited::default()
    }
}

/// What the host did: the user stopped the execution, and the host later
/// interrupted a runaway too.
fn both() -> Waited {
    Waited {
        user_stopped: true,
        ..runaway()
    }
}

fn interrupted(traceback: &str) -> Value {
    json!({"status": "error", "error": traceback})
}

// ---------------------------------------------------------------------------- the user

/// The wrong remedy for a suspended execution is a signal. A press cancels,
/// and a loop that answers gets nothing more.
#[tokio::test(start_paused = true)]
async fn a_press_on_a_suspended_execution_cancels_and_signals_nothing() {
    let presses = Presses::default();
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

    assert_eq!(presses.press(), Some(Press::Stop(id)));
    assert_eq!(fake.expect("cancel").await["id"], json!(id));
    turning(&mut fake).await;
    // What follows the answer is the next periodic check, not an interrupt.
    fake.expect("cpu").await;
    fake.result(id, interrupted("CancelledError")).await;
    let settled = within(settled).await.expect("settled");
    assert_eq!(settled.waited, user());
    assert!(
        matches!(settled.outcome, Outcome::Error { .. }),
        "{settled:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_press_on_a_quiet_loop_interrupts_by_what_its_thread_is_doing() {
    for (after, runaway) in [(3.0, true), (1.0, false)] {
        let presses = Presses::default();
        let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

        presses.press();
        fake.expect("cancel").await;
        quiet(&mut fake, 1.0, after).await;
        let request = fake.expect("interrupt").await;
        assert_eq!(request["id"], json!(id));
        assert_eq!(request["runaway"], runaway, "{request}");
        fake.result(id, interrupted("KeyboardInterrupt")).await;
        assert_eq!(within(settled).await.expect("settled").waited, user());
    }
}

/// A second press stops waiting. The execution keeps its slot, and its reply
/// arrives later as a late result of its own.
#[tokio::test(start_paused = true)]
async fn a_second_press_stops_waiting_and_the_slot_stays_held() {
    let presses = Presses::default();
    let (interpreter, mut fake, id, settled, waiting) = settling_one(&presses).await;

    presses.press();
    fake.expect("cancel").await;
    turning(&mut fake).await;
    assert_eq!(presses.press(), Some(Press::GiveUp(id)));
    let settled = within(settled).await.expect("settled");
    drop(waiting);
    assert_eq!(
        settled.outcome,
        Outcome::Unknown(Unknown::Unresolved { id })
    );
    assert_eq!(
        settled.waited,
        Waited {
            gave_up: Some(GaveUp::User),
            ..user()
        }
    );
    assert_eq!(interpreter.abandoned(), Some(id));

    let mut next = interpreter.submit("more()").expect("submitted");
    assert_eq!(
        within(next.outcome()).await,
        Outcome::Refused { holder: id }
    );
    fake.ok(id, "late\n").await;
    round_trip(&interpreter, &mut fake).await;
    let late = interpreter.take_late();
    assert_eq!(late.len(), 1, "{late:?}");
    assert_eq!(late[0].id, id);
}

/// A press counts against the execution it was made during. One that races
/// its outcome does not carry over to cancel the next.
#[tokio::test(start_paused = true)]
async fn a_press_that_races_an_outcome_does_not_reach_the_next_execution() {
    let (interpreter, mut fake) = Fake::connected().await;
    let presses = Presses::default();

    let first = submitted(&interpreter, &mut fake).await;
    let first_id = first.id();
    let waiting = presses.waiting_on(first_id);
    let settled = settling(&interpreter, first, &presses, paused());
    fake.ok(first_id, "done\n").await;
    within(settled).await.expect("settled");
    // Pressed after the outcome arrived, before the call let go of it.
    assert_eq!(presses.press(), Some(Press::Stop(first_id)));
    drop(waiting);

    let second = submitted(&interpreter, &mut fake).await;
    let id = second.id();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, second, &presses, paused());
    round_trip(&interpreter, &mut fake).await;
    fake.ok(id, "fine\n").await;
    assert_eq!(
        within(settled).await.expect("settled").waited,
        Waited::default()
    );
}

/// A second press during the check the first one started stops waiting now,
/// not once that check has run out its probe and CPU readings.
#[tokio::test(start_paused = true)]
async fn a_second_press_during_a_check_stops_waiting_at_once() {
    let presses = Presses::default();
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

    presses.press();
    fake.expect("cancel").await;
    cpu(&mut fake, 1.0).await;
    fake.expect("inv").await;
    let pressed = Instant::now();
    presses.press();
    let settled = within(settled).await.expect("settled");
    assert!(
        pressed.elapsed() <= paused().last_look,
        "took {:?}",
        pressed.elapsed()
    );
    assert_eq!(
        settled.outcome,
        Outcome::Unknown(Unknown::Unresolved { id })
    );
}

/// A press during one of the host's own checks is acted on at once, too: the
/// cancel goes out without waiting for the probe to run out.
#[tokio::test(start_paused = true)]
async fn a_press_during_an_automatic_check_is_acted_on_at_once() {
    let presses = Presses::default();
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

    cpu(&mut fake, 1.0).await;
    fake.expect("inv").await;
    let pressed = Instant::now();
    presses.press();
    fake.expect("cancel").await;
    assert_eq!(pressed.elapsed(), Duration::ZERO);
    // The user's check waits on the probe already out, rather than another.
    still_quiet(&mut fake, 1.0, 1.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], false);
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, user());
}

/// An answer to a probe an earlier check sent says the loop turned then, not
/// that it turns now. Here the loop turned between two blocking calls, and
/// the user's check must not take that answer for its own: it sends a fresh
/// probe, which goes unanswered, and interrupts the blocking call.
#[tokio::test(start_paused = true)]
async fn a_probe_answered_before_a_check_is_no_evidence_for_it() {
    let presses = Presses::default();
    let (interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

    let earlier = quiet(&mut fake, 1.0, 1.0).await;
    fake.inventory(&earlier["id"]).await;
    // Read by the host, in order, before anything after it.
    round_trip(&interpreter, &mut fake).await;

    presses.press();
    fake.expect("cancel").await;
    quiet(&mut fake, 1.0, 1.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], false);
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, user());
}

/// A user's stop is kept when the code survives it and the host then
/// interrupts it as a runaway, since that is what stops the turn's later
/// calls -- both when the runaway interrupt ends it, and when the host
/// finally stops waiting, which is then not the user asking twice.
#[tokio::test(start_paused = true)]
async fn a_user_stop_is_kept_through_a_later_runaway_interrupt() {
    for exhausted in [false, true] {
        let presses = Presses::default();
        let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

        presses.press();
        fake.expect("cancel").await;
        quiet(&mut fake, 1.0, 3.0).await;
        fake.expect("interrupt").await;
        let mut clock = 11.0;
        let rounds = if exhausted { ATTEMPTS } else { 1 };
        for _ in 0..rounds {
            still_quiet(&mut fake, clock, clock + 5.0).await;
            clock += 10.0;
            assert_eq!(fake.expect("interrupt").await["runaway"], true);
        }
        let expected = if exhausted {
            still_quiet(&mut fake, clock, clock + 5.0).await;
            Waited {
                gave_up: Some(GaveUp::Runaway),
                ..both()
            }
        } else {
            fake.ok(id, "survived\n").await;
            both()
        };
        assert_eq!(within(settled).await.expect("settled").waited, expected);
    }
}

// ---------------------------------------------------------------------------- on its own

/// A loop that answers is healthy; one that is quiet with its thread idle --
/// a synchronous `subprocess.run`, say -- is left alone. And the probe it has
/// not answered is waited on again, not sent again, so a long wait does not
/// leave a queue of them for the loop to answer when it returns.
#[tokio::test(start_paused = true)]
async fn a_loop_that_answers_or_is_blocked_is_left_alone() {
    let (interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    turning(&mut fake).await;
    quiet(&mut fake, 1.0, 1.02).await;
    still_quiet(&mut fake, 1.02, 1.02).await;
    still_quiet(&mut fake, 1.02, 1.02).await;
    fake.ok(id, "built\n").await;
    let settled = within(settled).await.expect("settled");
    assert_eq!(settled.waited, Waited::default());
    round_trip(&interpreter, &mut fake).await;
}

/// Quiet and busy is a runaway, and is interrupted. But an interrupt proves
/// nothing about this execution -- it may have ended a task another left
/// spinning -- so the host checks again and, finding the loop turning, goes
/// on waiting for an execution that is not unresolved.
#[tokio::test(start_paused = true)]
async fn a_runaway_is_interrupted_and_then_checked_again() {
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    let probe = quiet(&mut fake, 1.0, 6.0).await;
    let request = fake.expect("interrupt").await;
    assert_eq!(request["runaway"], true, "{request}");
    turning_again(&mut fake, &probe).await;
    turning(&mut fake).await;
    fake.ok(id, "survived\n").await;
    let settled = within(settled).await.expect("settled");
    assert!(matches!(settled.outcome, Outcome::Ok(_)), "{settled:?}");
    assert_eq!(settled.waited, runaway());
}

/// Beside other CPU-bound threads a spinning loop gets only its share of the
/// GIL. A quarter of a CPU is still a runaway.
#[tokio::test(start_paused = true)]
async fn a_runaway_sharing_the_gil_is_still_a_runaway() {
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    quiet(&mut fake, 1.0, 1.0 + 0.25 * 5.0).await;
    fake.expect("interrupt").await;
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, runaway());
}

#[tokio::test(start_paused = true)]
async fn a_runaway_that_survives_its_interrupts_is_given_up() {
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    quiet(&mut fake, 1.0, 6.0).await;
    fake.expect("interrupt").await;
    let mut clock = 11.0;
    for _ in 1..ATTEMPTS {
        still_quiet(&mut fake, clock, clock + 5.0).await;
        clock += 10.0;
        fake.expect("interrupt").await;
    }
    still_quiet(&mut fake, clock, clock + 5.0).await;
    let settled = within(settled).await.expect("settled");
    assert_eq!(
        settled.outcome,
        Outcome::Unknown(Unknown::Unresolved { id })
    );
    assert_eq!(
        settled.waited,
        Waited {
            gave_up: Some(GaveUp::Runaway),
            ..runaway()
        }
    );
}

/// Native code holding the GIL starves the thread that answers for the CPU
/// clock too. Nothing can interrupt it, and it may be long rather than
/// endless, so the host waits.
#[tokio::test(start_paused = true)]
async fn a_starved_interpreter_is_waited_for() {
    let (interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    fake.expect("cpu").await;
    fake.expect("cpu").await;
    fake.ok(id, "sorted\n").await;
    assert_eq!(
        within(settled).await.expect("settled").waited,
        Waited::default()
    );
    round_trip(&interpreter, &mut fake).await;
}

/// A submission refused behind an execution nobody waits for any more is the
/// only chance the host has to look at that execution again.
#[tokio::test(start_paused = true)]
async fn a_refusal_behind_an_abandoned_execution_checks_it() {
    let (interpreter, mut fake) = Fake::connected().await;
    let first = submitted(&interpreter, &mut fake).await;
    let holder = first.id();
    assert_eq!(
        first.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id: holder })
    );

    let second = interpreter.submit("more()").expect("submitted");
    let settled = settling(&interpreter, second, &Presses::default(), paused());
    quiet(&mut fake, 1.0, 6.0).await;
    let request = fake.expect("interrupt").await;
    assert_eq!(request["id"], json!(holder));
    let settled = within(settled).await.expect("settled");
    assert_eq!(settled.outcome, Outcome::Refused { holder });
    assert_eq!(
        settled.waited,
        Waited {
            holder: Some(Verdict::Spinning),
            ..Waited::default()
        }
    );
}

/// With nobody waiting, the user's stop reaches the execution holding the
/// slot: a cancel, and an interrupt aimed at its own code, with no check
/// between. Once it has replied there is nothing left to stop.
#[tokio::test(start_paused = true)]
async fn a_stop_with_nobody_waiting_cancels_the_holder_and_aims_an_interrupt_at_it() {
    let (interpreter, mut fake) = Fake::connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    let holder = execution.id();
    assert_eq!(
        execution.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id: holder })
    );

    assert_eq!(stop_abandoned(&interpreter), Some(holder));
    assert_eq!(fake.expect("cancel").await["id"], json!(holder));
    let request = fake.expect("interrupt").await;
    assert_eq!(request["id"], json!(holder));
    assert_eq!(request["runaway"], false, "{request}");

    fake.result(holder, interrupted("CancelledError")).await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(stop_abandoned(&interpreter), None);
    // Nothing more was sent: the next request is the next submission.
    let next = interpreter.submit("more()").expect("submitted");
    fake.exec(next.id()).await;
}

/// A stop is for an execution nobody waits for. One a call is still waiting
/// on is that call's to stop, through its presses, and is left alone.
#[tokio::test(start_paused = true)]
async fn a_stop_does_not_reach_an_execution_a_call_still_waits_on() {
    let (interpreter, mut fake) = Fake::connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    assert_eq!(stop_abandoned(&interpreter), None);
    // Nothing was sent: the next request the fake sees is the round trip's.
    round_trip(&interpreter, &mut fake).await;
    fake.ok(execution.id(), "done\n").await;
}

// ---------------------------------------------------------------------------- the real interpreter

async fn settle_real(interpreter: &Interpreter, source: &str) -> (ExecId, Settled) {
    let execution = interpreter.submit(source).expect("submitted");
    let id = execution.id();
    let settled = within(settle(
        interpreter,
        execution,
        &Presses::default(),
        &quick(),
    ))
    .await;
    (id, settled)
}

fn traceback(settled: &Settled) -> &str {
    match &settled.outcome {
        Outcome::Error { traceback, .. } => traceback,
        other => panic!("expected an error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_wedge_is_recovered_with_nobody_asking() {
    let interpreter = start_on_host().await;
    let (_, settled) = settle_real(&interpreter, "while True: pass").await;
    assert_eq!(settled.waited, runaway());
    assert!(
        traceback(&settled).contains("KeyboardInterrupt"),
        "{settled:?}"
    );
    assert_eq!(
        settle_real(&interpreter, "1 + 1").await.1.outcome,
        ok("2\n")
    );
}

/// Code can catch the interrupt the host sends a runaway, and finish. It is
/// still recorded as interrupted, and the outcome is its own.
#[tokio::test]
async fn a_runaway_that_catches_its_interrupt_finishes() {
    let interpreter = start_on_host().await;
    let (_, settled) = settle_real(
        &interpreter,
        "try:\n    while True: pass\nexcept KeyboardInterrupt:\n    pass\n'caught'",
    )
    .await;
    assert_eq!(settled.outcome, ok("'caught'\n"));
    assert_eq!(settled.waited, runaway());
}

/// A synchronous `subprocess.run` stops the loop answering while being
/// perfectly healthy. Checked many times over, it is never interrupted.
#[tokio::test]
async fn a_blocking_subprocess_run_is_left_to_finish() {
    let interpreter = start_on_host().await;
    let (_, settled) = settle_real(
        &interpreter,
        "import subprocess\nsubprocess.run(['sleep', '2']).returncode",
    )
    .await;
    assert_eq!(settled.waited, Waited::default());
    assert_eq!(settled.outcome, ok("0\n"));
}

/// A task an earlier execution left running wedges the loop while this one is
/// suspended. The host interrupts the task, not this execution, which goes on
/// to finish -- and is not recorded unresolved for having been waited on
/// through it.
#[tokio::test]
async fn a_background_wedge_is_ended_and_the_execution_it_stalled_finishes() {
    let interpreter = start_on_host().await;
    let setup = "go = asyncio.Event()\n\
                 async def spin():\n    await go.wait()\n    while True: pass\n\
                 spinner = asyncio.create_task(spin())";
    let (started_it, set_up) = settle_real(&interpreter, setup).await;
    assert!(matches!(set_up.outcome, Outcome::Ok(_)), "{set_up:?}");

    let (_, settled) =
        settle_real(&interpreter, "go.set()\nawait asyncio.sleep(1)\n'survived'").await;
    assert_eq!(settled.waited, runaway());
    let Outcome::Ok(report) = &settled.outcome else {
        panic!("expected it to finish, got {settled:?}");
    };
    assert_eq!(report.output, "'survived'\n");
    let billed: Vec<_> = report.background.iter().map(|entry| entry.id).collect();
    assert_eq!(
        billed,
        [started_it],
        "the task's death, billed to its starter: {report:?}"
    );
    assert!(
        report.background[0].output.contains("KeyboardInterrupt"),
        "{report:?}"
    );
}

/// Ctrl-C on a bare await on something that never resolves: cancelled, the
/// slot freed, and the next submission runs.
#[tokio::test]
async fn a_press_ends_an_await_that_never_resolves() {
    let interpreter = start_on_host().await;
    let presses = Presses::default();
    let execution = interpreter
        .submit("await asyncio.get_running_loop().create_future()")
        .expect("submitted");
    let _waiting = presses.waiting_on(execution.id());
    let settled = settling(&interpreter, execution, &presses, quick());
    // Once the loop has run the body to its await.
    within(interpreter.inventory()).await.expect("an inventory");
    presses.press();
    let settled = within(settled).await.expect("settled");
    assert_eq!(settled.waited, user());
    assert!(
        traceback(&settled).contains("CancelledError"),
        "{settled:?}"
    );
    assert_eq!(
        settle_real(&interpreter, "1 + 1").await.1.outcome,
        ok("2\n")
    );
}

/// The case of #468: code that catches its cancellation and waits again holds
/// the slot once a second press gives up on it, and nothing a call does can
/// reach it. A stop with nobody waiting cancels it again, the slot comes back
/// with its reply a late result, and the next submission runs.
#[tokio::test]
async fn a_stop_ends_an_execution_that_caught_its_first_cancel() {
    let interpreter = start_on_host().await;
    let presses = Presses::default();
    let execution = interpreter
        .submit(
            "try:\n    await asyncio.get_running_loop().create_future()\n\
             except asyncio.CancelledError:\n    pass\n\
             await asyncio.get_running_loop().create_future()",
        )
        .expect("submitted");
    let id = execution.id();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, &presses, quick());
    // Once the loop has run the body to its first await.
    within(interpreter.inventory()).await.expect("an inventory");
    presses.press();
    // Once the cancel has been delivered, and caught.
    within(interpreter.inventory()).await.expect("an inventory");
    presses.press();
    let settled = within(settled).await.expect("settled");
    assert_eq!(
        settled.outcome,
        Outcome::Unknown(Unknown::Unresolved { id })
    );

    assert_eq!(stop_abandoned(&interpreter), Some(id));
    slot_freed(&interpreter).await;
    let late = interpreter.take_late();
    assert_eq!(late.len(), 1, "{late:?}");
    assert_eq!(late[0].id, id);
    assert!(
        matches!(&late[0].outcome, Outcome::Error { traceback, .. } if traceback.contains("CancelledError")),
        "{late:?}"
    );
    assert_eq!(
        settle_real(&interpreter, "1 + 1").await.1.outcome,
        ok("2\n")
    );
}

// ---------------------------------------------------------------------------- the event log

/// What the host did while it waited is recorded as it did it: what the
/// probe found the namespace holding, each probe the loop did not answer, each
/// interrupt sent, and the giving up -- and after it, the user's stop of what
/// was abandoned.
#[tokio::test(start_paused = true)]
async fn what_the_host_did_while_waiting_is_recorded() {
    let dir = tempfile::tempdir().expect("a log dir");
    let events = opened(dir.path()).await;
    let (interpreter, mut fake) = Fake::connected_with(events.clone()).await;
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let presses = Presses::default();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, &presses, paused());

    turning(&mut fake).await;
    quiet(&mut fake, 1.0, 6.0).await;
    fake.expect("interrupt").await;
    let mut clock = 11.0;
    for _ in 1..ATTEMPTS {
        still_quiet(&mut fake, clock, clock + 5.0).await;
        clock += 10.0;
        fake.expect("interrupt").await;
    }
    still_quiet(&mut fake, clock, clock + 5.0).await;
    within(settled).await.expect("settled");
    assert_eq!(stop_abandoned(&interpreter), Some(id));
    fake.expect("cancel").await;
    fake.expect("interrupt").await;
    // The log is closed on a running clock, or its deadline passes at once.
    tokio::time::resume();
    events.close().await.expect("nothing lost");

    let records = recorded(dir.path());
    let mut expected = vec!["exec.submitted", "inventory.observed"];
    for _ in 0..ATTEMPTS {
        expected.extend(["exec.probe.failed", "exec.interrupt.sent"]);
    }
    expected.extend(["exec.probe.failed", "exec.abandoned"]);
    expected.extend(["exec.cancel.sent", "exec.interrupt.sent"]);
    assert_eq!(kinds(&records), expected);
    assert_eq!(
        *of_kind(&records, "inventory.observed")[0],
        json!({"execid": id, "names": [], "total": 0, "more": 0})
    );
    assert_eq!(
        *of_kind(&records, "exec.probe.failed")[0],
        json!({"execid": id, "verdict": "spinning"})
    );
    assert_eq!(
        *of_kind(&records, "exec.interrupt.sent")[0],
        json!({"execid": id, "runaway": true})
    );
    assert_eq!(
        *of_kind(&records, "exec.abandoned")[0],
        json!({"execid": id, "why": "runaway"})
    );
}
