//! The watch over the slot and the wait on the call, driven two ways. Against a
//! fake transport with time paused, for exactly what the host sends given what
//! the interpreter answers -- above all, what it does *not* send. Against the
//! real interpreter with short timings, for the shapes it exists to survive.
//!
//! The watch starts with the slot, so every submission here is checked on its
//! own account, and a test sets the interpreter's timings before it submits.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::super::host::{ExecId, Execution, Interpreter, Outcome, PRIMARY, Unknown};
use super::super::testing::{Fake, ok, round_trip, slot_freed, start_on_host, within};
use super::{
    ATTEMPTS, Found, GaveUp, Press, Presses, Settled, Timings, Verdict, Waited, Waiting, settle,
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

/// A fake interpreter whose checks are [`paused`] apart.
async fn connected() -> (Interpreter, Fake) {
    let (interpreter, fake) = Fake::connected().await;
    interpreter.set_timings(paused());
    (interpreter, fake)
}

/// The real interpreter, on the host, checked [`quick`]ly.
async fn started() -> Interpreter {
    let interpreter = start_on_host().await;
    interpreter.set_timings(quick());
    interpreter
}

fn settling(
    interpreter: &Interpreter,
    execution: Execution,
    presses: &Presses,
) -> JoinHandle<Settled> {
    let (interpreter, presses) = (interpreter.clone(), presses.clone());
    tokio::spawn(async move { settle(&interpreter, execution, &presses).await })
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

/// The same, for a check the watch makes while its earlier probe is still
/// unanswered: that one is waited on again rather than another sent.
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
    let (interpreter, mut fake) = connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, presses);
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
    let (interpreter, mut fake) = connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let mut watched = execution.watched();
    let waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, &presses);

    presses.press();
    fake.expect("cancel").await;
    turning(&mut fake).await;
    // A press wins over a check still running, so it is pressed once this one
    // has run its course.
    within(watched.wait_for(|watched| watched.verdict.is_some()))
        .await
        .expect("recorded");
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

    // Refused at once with what the first press's check found.
    let mut next = interpreter.submit("more()").expect("submitted");
    assert_eq!(
        within(next.outcome()).await,
        Outcome::Refused {
            holder: id,
            checked: Some(Verdict::Turning)
        }
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
    let (interpreter, mut fake) = connected().await;
    let presses = Presses::default();

    let first = submitted(&interpreter, &mut fake).await;
    let first_id = first.id();
    let waiting = presses.waiting_on(first_id);
    let settled = settling(&interpreter, first, &presses);
    fake.ok(first_id, "done\n").await;
    within(settled).await.expect("settled");
    // Pressed after the outcome arrived, before the call let go of it.
    assert_eq!(presses.press(), Some(Press::Stop(first_id)));
    drop(waiting);

    let second = submitted(&interpreter, &mut fake).await;
    let id = second.id();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, second, &presses);
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

/// A press during one of the watch's checks is acted on at once, too: the
/// cancel goes out without waiting for the probe to run out. The user's check is
/// its own, with readings and a probe of its own beside the watch's.
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
    quiet(&mut fake, 1.0, 1.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], false);
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, user());
}

/// An answer to a probe an earlier check sent says the loop turned then, not
/// that it turns now. Here the loop turned between two blocking calls, and the
/// next check must not take that answer for its own: it sends a fresh probe,
/// which goes unanswered, and finds the loop spinning in the second call.
#[tokio::test(start_paused = true)]
async fn a_probe_answered_before_a_check_is_no_evidence_for_it() {
    let (interpreter, mut fake, id, settled, _waiting) = settling_one(&Presses::default()).await;

    let earlier = quiet(&mut fake, 1.0, 1.0).await;
    fake.inventory(&earlier["id"]).await;
    // Read by the host, in order, before anything after it.
    round_trip(&interpreter, &mut fake).await;

    quiet(&mut fake, 1.0, 6.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], true);
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, runaway());
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
        // The user's interrupt is no attempt: the watch's own checks begin
        // with theirs, and each of them is one.
        quiet(&mut fake, 11.0, 16.0).await;
        assert_eq!(fake.expect("interrupt").await["runaway"], true);
        let mut clock = 21.0;
        let rounds = if exhausted { ATTEMPTS } else { 1 };
        for _ in 1..rounds {
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

// ---------------------------------------------------------------------------- beside one another

/// The watch and a press check on their own, and an interrupt one sends can land
/// in the middle of the other's CPU reading. The thread was spinning, a press
/// interrupts it, and it goes into a `finally:` that sleeps: the watch's reading
/// is three seconds of spinning in five, and would interrupt it out of its
/// cleanup. It is read again instead, after the interrupt, and finds the thread
/// idle.
#[tokio::test(start_paused = true)]
async fn a_check_an_interrupt_landed_in_is_run_again_not_acted_on() {
    let dir = tempfile::tempdir().expect("a log dir");
    let events = opened(dir.path()).await;
    let presses = Presses::default();
    let (interpreter, mut fake) = Fake::connected_with(events.clone()).await;
    interpreter.set_timings(paused());
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let mut watched = execution.watched();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, &presses);

    // The watch's check begins, and its probe goes unanswered.
    cpu(&mut fake, 10.0).await;
    fake.expect("inv").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The press, a second later, finds the thread spinning two seconds after
    // that, and interrupts it.
    presses.press();
    fake.expect("cancel").await;
    quiet(&mut fake, 11.0, 13.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], true);
    // The watch's window ends with the thread idle since the interrupt. That
    // check is not acted on: the next one is of the thread as it is now, and
    // keeps its probe.
    cpu(&mut fake, 13.0).await;
    still_quiet(&mut fake, 13.0, 13.0).await;
    within(watched.wait_for(|watched| watched.verdict == Some(Verdict::Blocked)))
        .await
        .expect("recorded");
    // Nothing was sent for the stale one: the next request is the round
    // trip's.
    round_trip(&interpreter, &mut fake).await;

    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, user());

    // The log has the press's finding and the watch's second one, and not the
    // reading the interrupt spoiled.
    tokio::time::resume();
    events.close().await.expect("nothing lost");
    let records = recorded(dir.path());
    let verdicts: Vec<_> = of_kind(&records, "exec.probe.failed")
        .iter()
        .map(|record| record["verdict"].clone())
        .collect();
    assert_eq!(
        verdicts,
        [json!("spinning"), json!("blocked")],
        "{records:?}"
    );
}

/// The same the other way about: the watch interrupts a runaway while a press's
/// check is taking its reading, and what that reads is not a thread still
/// spinning. The press's check is run again, finds the thread blocked, and
/// interrupts it as that, not as the runaway it no longer is.
#[tokio::test(start_paused = true)]
async fn a_press_whose_check_an_interrupt_landed_in_checks_again() {
    let presses = Presses::default();
    let (_interpreter, mut fake, id, settled, _waiting) = settling_one(&presses).await;

    cpu(&mut fake, 10.0).await;
    fake.expect("inv").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    presses.press();
    fake.expect("cancel").await;
    cpu(&mut fake, 14.0).await;
    fake.expect("inv").await;
    // The watch's window ends, and it interrupts the runaway it finds.
    cpu(&mut fake, 15.0).await;
    assert_eq!(fake.expect("interrupt").await["runaway"], true);
    // The press's window ends with the thread idle since then.
    cpu(&mut fake, 15.0).await;
    still_quiet(&mut fake, 15.0, 15.0).await;
    let request = fake.expect("interrupt").await;
    assert_eq!(request["runaway"], false, "{request}");
    fake.result(id, interrupted("KeyboardInterrupt")).await;
    assert_eq!(within(settled).await.expect("settled").waited, both());
}

/// An interrupt from anywhere is one: a stop with nobody waiting aims one at the
/// holder, and the watch's check that began before it is not acted on either.
#[tokio::test(start_paused = true)]
async fn a_stop_with_nobody_waiting_makes_a_check_it_landed_in_stale() {
    let (interpreter, mut fake) = connected().await;
    let first = submitted(&interpreter, &mut fake).await;
    let holder = first.id();
    let mut watched = first.watched();
    first.stop_waiting();

    cpu(&mut fake, 10.0).await;
    fake.expect("inv").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(stop_abandoned(&interpreter), Some(holder));
    fake.expect("cancel").await;
    assert_eq!(fake.expect("interrupt").await["runaway"], false);
    cpu(&mut fake, 11.0).await;
    still_quiet(&mut fake, 11.0, 11.0).await;
    within(watched.wait_for(|watched| watched.verdict == Some(Verdict::Blocked)))
        .await
        .expect("recorded");
    round_trip(&interpreter, &mut fake).await;
}

/// The decision is held to the count too, not only the verdict: an interrupt
/// that gets in between a check's being recorded and the watch's acting on it
/// stops the watch's own, and one sent after nothing else has gone is sent.
#[tokio::test(start_paused = true)]
async fn the_watchs_interrupt_is_sent_only_if_none_came_since_its_check_began() {
    let (interpreter, mut fake) = connected().await;
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let seen = interpreter.interrupts(id).expect("holds the slot");

    interpreter.interrupt(id, false);
    assert_eq!(fake.expect("interrupt").await["runaway"], false);
    assert!(!interpreter.interrupt_runaway(id, seen));
    assert!(!execution.watched().borrow().interrupted);
    assert_eq!(interpreter.found(id, seen, Verdict::Spinning), Found::Stale);
    // Not a CPU reading, so there is nothing in it for an interrupt to spoil.
    assert_eq!(
        interpreter.found(id, seen, Verdict::Turning),
        Found::Recorded
    );

    assert!(interpreter.interrupt_runaway(id, seen + 1));
    assert_eq!(fake.expect("interrupt").await["runaway"], true);
    assert!(execution.watched().borrow().interrupted);
    assert_eq!(interpreter.interrupts(id), Some(seen + 2));
    // Nothing else was sent: the next request is the round trip's.
    round_trip(&interpreter, &mut fake).await;
}

// ---------------------------------------------------------------------------- nobody waiting

/// An execution nobody waits for any more is still watched: found spinning, it
/// is interrupted with no submission to prompt the look.
#[tokio::test(start_paused = true)]
async fn an_abandoned_runaway_is_interrupted_with_nobody_asking() {
    let (interpreter, mut fake) = connected().await;
    let first = submitted(&interpreter, &mut fake).await;
    let holder = first.id();
    assert_eq!(
        first.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id: holder })
    );

    quiet(&mut fake, 1.0, 6.0).await;
    let request = fake.expect("interrupt").await;
    assert_eq!(request["id"], json!(holder));
    assert_eq!(request["runaway"], true, "{request}");
    fake.result(holder, interrupted("KeyboardInterrupt")).await;
    round_trip(&interpreter, &mut fake).await;
    assert_eq!(interpreter.abandoned(), None);
    assert_eq!(interpreter.take_late().len(), 1);
}

/// A submission refused behind an abandoned execution reads what the last
/// check found, at once: it waits on no probe, and sends nothing. Behind one a
/// call still waits on, it carries no verdict, whatever was found.
#[tokio::test(start_paused = true)]
async fn a_refusal_behind_an_abandoned_execution_reads_the_last_check_at_once() {
    let (interpreter, mut fake) = connected().await;
    let first = submitted(&interpreter, &mut fake).await;
    let holder = first.id();
    let mut watched = first.watched();
    let refused = |checked| Outcome::Refused { holder, checked };

    // Before any check there is nothing to read.
    assert_eq!(
        within(interpreter.submit("more()").expect("submitted").outcome()).await,
        refused(None)
    );
    // The watch's first check, with a call still waiting.
    turning(&mut fake).await;
    within(watched.wait_for(|watched| watched.verdict.is_some()))
        .await
        .expect("recorded");
    assert_eq!(
        within(interpreter.submit("more()").expect("submitted").outcome()).await,
        refused(None)
    );

    assert_eq!(
        first.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id: holder })
    );
    let refusal = Instant::now();
    assert_eq!(
        within(interpreter.submit("more()").expect("submitted").outcome()).await,
        refused(Some(Verdict::Turning))
    );
    assert_eq!(refusal.elapsed(), Duration::ZERO);
    // Nothing was sent for it: the next request is the round trip's.
    round_trip(&interpreter, &mut fake).await;

    // A later check replaces what was found.
    quiet(&mut fake, 1.0, 6.0).await;
    assert_eq!(fake.expect("interrupt").await["id"], json!(holder));
    assert_eq!(
        within(interpreter.submit("more()").expect("submitted").outcome()).await,
        refused(Some(Verdict::Spinning))
    );
    round_trip(&interpreter, &mut fake).await;
}

/// Giving up is not ceasing to watch. A runaway the host stopped waiting for is
/// checked again a `check_every` on -- not a `grace` -- and interrupted again,
/// with nobody waiting.
#[tokio::test(start_paused = true)]
async fn a_runaway_the_host_gave_up_on_is_still_interrupted() {
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

    let gave_up = Instant::now();
    still_quiet(&mut fake, clock + 10.0, clock + 15.0).await;
    let request = fake.expect("interrupt").await;
    assert_eq!(request["id"], json!(id));
    assert_eq!(request["runaway"], true, "{request}");
    assert_eq!(
        gave_up.elapsed(),
        paused().check_every + paused().probe,
        "a check, then its probe"
    );

    // And a whole `check_every` after that one, since nobody waits for a
    // `grace` to see what it did.
    let interrupted = Instant::now();
    fake.expect("cpu").await;
    assert_eq!(interrupted.elapsed(), paused().check_every);
}

/// What a check found is recorded for the execution it checked, and only while
/// that one holds the slot: a check that finishes as its execution replies is
/// not recorded against the next one.
#[tokio::test(start_paused = true)]
async fn a_check_that_outlasts_its_execution_is_not_recorded_against_the_next() {
    let (interpreter, mut fake) = connected().await;
    let first = submitted(&interpreter, &mut fake).await;
    let first_id = first.id();
    fake.ok(first_id, "done\n").await;
    // Read by the host, in order, so the slot is free.
    round_trip(&interpreter, &mut fake).await;
    let second = submitted(&interpreter, &mut fake).await;

    assert_eq!(interpreter.interrupts(first_id), None);
    assert_eq!(
        interpreter.found(first_id, 0, Verdict::Spinning),
        Found::Freed
    );
    assert_eq!(second.watched().borrow().verdict, None);
    assert_eq!(
        interpreter.found(second.id(), 0, Verdict::Blocked),
        Found::Recorded
    );
    assert_eq!(second.watched().borrow().verdict, Some(Verdict::Blocked));
}

/// The watch ends when its slot is free: nothing is checked for an execution
/// that has replied.
#[tokio::test(start_paused = true)]
async fn the_watch_ends_with_its_slot() {
    let (interpreter, mut fake) = connected().await;
    let mut execution = submitted(&interpreter, &mut fake).await;
    fake.ok(execution.id(), "done\n").await;
    assert_eq!(within(execution.outcome()).await, ok("done\n"));

    // Well past when it would have checked.
    tokio::time::sleep(paused().check_every * 3).await;
    // Nothing was sent: the next request is the round trip's.
    round_trip(&interpreter, &mut fake).await;
}

/// With nobody waiting, the user's stop reaches the execution holding the
/// slot: a cancel, and an interrupt aimed at its own code, with no check
/// between. Once it has replied there is nothing left to stop.
#[tokio::test(start_paused = true)]
async fn a_stop_with_nobody_waiting_cancels_the_holder_and_aims_an_interrupt_at_it() {
    let (interpreter, mut fake) = connected().await;
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
    let (interpreter, mut fake) = connected().await;
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
    let settled = within(settle(interpreter, execution, &Presses::default())).await;
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
    let interpreter = started().await;
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

/// A wedge whose call stopped waiting for it: nothing asks, and no submission
/// prompts a look, yet the host interrupts it, and its reply comes back late.
#[tokio::test]
async fn an_abandoned_wedge_is_recovered_with_nobody_waiting() {
    let interpreter = started().await;
    let execution = interpreter.submit("while True: pass").expect("submitted");
    let id = execution.id();
    assert_eq!(
        execution.stop_waiting(),
        Outcome::Unknown(Unknown::Unresolved { id })
    );

    slot_freed(&interpreter).await;
    let late = interpreter.take_late();
    assert_eq!(late.len(), 1, "{late:?}");
    assert_eq!(late[0].id, id);
    assert!(
        matches!(&late[0].outcome, Outcome::Error { traceback, .. } if traceback.contains("KeyboardInterrupt")),
        "{late:?}"
    );
    assert_eq!(
        settle_real(&interpreter, "1 + 1").await.1.outcome,
        ok("2\n")
    );
}

/// What watches the slot holds no handle that counts. With every other handle
/// dropped, the interpreter still ends -- its slot held, and checked -- rather
/// than being kept alive by the task that checks it.
#[tokio::test]
async fn dropping_every_handle_ends_an_interpreter_whose_slot_is_watched() {
    let interpreter = started().await;
    let execution = interpreter
        .submit("import time\ntime.sleep(60)")
        .expect("submitted");
    let mut watched = execution.watched();
    // Once the watch has checked the loop it cannot reach, and left it alone.
    let verdict = within(watched.wait_for(|watched| watched.verdict.is_some()))
        .await
        .expect("watching")
        .verdict;
    assert_eq!(verdict, Some(Verdict::Blocked));

    let gone = interpreter.gone();
    drop(interpreter);
    within(gone).await;
    drop(execution);
}

/// Code can catch the interrupt the host sends a runaway, and finish. It is
/// still recorded as interrupted, and the outcome is its own.
#[tokio::test]
async fn a_runaway_that_catches_its_interrupt_finishes() {
    let interpreter = started().await;
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
    let interpreter = started().await;
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
    let interpreter = started().await;
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
    let interpreter = started().await;
    let presses = Presses::default();
    let execution = interpreter
        .submit("await asyncio.get_running_loop().create_future()")
        .expect("submitted");
    let _waiting = presses.waiting_on(execution.id());
    let settled = settling(&interpreter, execution, &presses);
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
    let interpreter = started().await;
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
    let settled = settling(&interpreter, execution, &presses);
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
    interpreter.set_timings(paused());
    let execution = submitted(&interpreter, &mut fake).await;
    let id = execution.id();
    let presses = Presses::default();
    let _waiting = presses.waiting_on(id);
    let settled = settling(&interpreter, execution, &presses);

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
