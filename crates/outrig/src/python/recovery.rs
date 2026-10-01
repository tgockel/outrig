//! Waiting for an execution, and what the host does when one will not end on
//! its own.
//!
//! Two failures need two remedies, and applying the wrong one is not harmless
//! (`plan/phase/0003-python/runtime-protection.md`). A loop that has stopped
//! turning -- a wedge -- takes an interrupt, which the interpreter raises
//! without going through the loop. An execution suspended on an await that
//! never resolves takes a cancel, which goes through the loop, since that is
//! where a task is cancelled. The host tells them apart by asking twice:
//! whether the loop answers ([`Interpreter::inventory`]), and whether the
//! thread running it is using a CPU ([`Interpreter::cpu`]).
//!
//! # When the host acts on its own
//!
//! A long execution keeps its slot for as long as it runs. Every
//! [`Timings::check_every`] the host checks, and it interrupts on its own only
//! when the loop does not answer *and* the thread burned CPU while it did not:
//! a runaway. A loop that does not answer while its thread is idle is blocked
//! in a system call -- a synchronous `subprocess.run`, a `time.sleep`, a
//! blocking read -- which may be perfectly healthy, and is left alone. So is a
//! suspended execution: the host never cancels one on its own.
//!
//! An interrupt is not proof of anything. The loop can resume between the
//! check and the signal, and the interrupt may land in a task another
//! execution left running rather than in this one, so the host checks again
//! after [`Timings::grace`] rather than concluding. It gives up -- the
//! execution becomes [`Unresolved`](super::host::Unknown::Unresolved), keeping its slot -- only after
//! [`ATTEMPTS`] interrupts have each left the loop spinning.
//!
//! # When the user asks
//!
//! The first press cancels, which is cheap and safe, and checks: a loop that
//! does not answer is interrupted too -- anything spinning on it, or the
//! execution's own blocking call. Nothing else is gated on evidence: a person
//! may stop healthy work. A later press on the same execution stops waiting
//! for it. Each press is counted against the execution it was made during, so
//! none reaches the next one.
//!
//! # When the interpreter cannot answer at all
//!
//! Native code that holds the GIL starves the thread that answers
//! [`Interpreter::cpu`] as well as the loop, and nothing can interrupt it.
//! The host says so once and keeps waiting, since the call may be long rather
//! than endless; the user can stop waiting.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::{Instant, sleep_until, timeout};

use super::host::{ExecId, Execution, Interpreter, InterpreterError, Inventory, Outcome};
use crate::events::{Event, Held};

/// The share of a CPU the loop's thread must have used across a probe that
/// went unanswered for the loop to count as spinning.
///
/// A thread blocked in a system call measures 0.000. A spinning one measures
/// 1.0 alone but only its share of the GIL beside other CPU-bound threads --
/// 0.505, 0.336, and 0.252 beside one, two, and three, measured on the
/// payload -- so the line sits far below the first of those and far above
/// idle.
const BUSY: f64 = 0.1;

/// How many interrupts a runaway gets, each followed by a check that finds
/// the loop still spinning, before the host stops waiting for it.
pub(crate) const ATTEMPTS: u32 = 3;

/// How long each step of waiting takes. A test shortens them.
#[derive(Debug, Clone)]
pub(crate) struct Timings {
    /// From submission to the first check, and between checks.
    pub(crate) check_every: Duration,
    /// How long a check waits for the loop to answer.
    pub(crate) probe: Duration,
    /// The same, when the user asked: a person is waiting on it.
    pub(crate) user_probe: Duration,
    /// How long a CPU reading may take. One that takes longer means nothing
    /// in the interpreter can run.
    pub(crate) cpu: Duration,
    /// From an interrupt to the check that sees what it did.
    pub(crate) grace: Duration,
    /// How long a press that gives up still waits for an outcome that may be
    /// about to arrive, so a cancel landing now is not reported as unknown.
    pub(crate) last_look: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            check_every: Duration::from_secs(30),
            probe: Duration::from_secs(5),
            user_probe: Duration::from_secs(2),
            cpu: Duration::from_secs(2),
            grace: Duration::from_secs(5),
            last_look: Duration::from_secs(1),
        }
    }
}

/// What the host did about an execution while it waited. Each is recorded on
/// its own, since one does not undo another: a user's stop that the code
/// survived is still a stop when the host later interrupts it as a runaway.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Waited {
    /// The user asked it to stop: a cancel, and an interrupt if the loop was
    /// not turning.
    pub(crate) user_stopped: bool,
    /// The loop stopped answering while its thread kept a CPU busy, and the
    /// host interrupted whatever agent code was spinning on it -- this
    /// execution's, or a task another left running.
    pub(crate) runaway_interrupted: bool,
    /// Why the host stopped waiting, when it did.
    pub(crate) gave_up: Option<GaveUp>,
    /// Refused behind an execution nobody is waiting for any more: what a
    /// check found that execution doing. One found spinning was interrupted.
    pub(crate) holder: Option<Verdict>,
}

/// Why the host stopped waiting, leaving the outcome
/// [`Unresolved`](super::host::Unknown::Unresolved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GaveUp {
    /// The user asked again.
    User,
    /// The loop kept spinning through [`ATTEMPTS`] interrupts.
    Runaway,
}

/// What a check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The loop answered: nothing holds it, and an execution is suspended on
    /// an await if it is running at all.
    Turning,
    /// The loop did not answer and its thread was idle: blocked in a system
    /// call.
    Blocked,
    /// The loop did not answer and its thread kept a CPU busy.
    Spinning,
    /// Nothing in the interpreter answered: native code holds the GIL.
    Starved,
}

impl Verdict {
    /// As the event log names it.
    fn name(self) -> &'static str {
        match self {
            Verdict::Turning => "turning",
            Verdict::Blocked => "blocked",
            Verdict::Spinning => "spinning",
            Verdict::Starved => "starved",
        }
    }
}

/// An execution's outcome, and what the host did while waiting for it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settled {
    pub(crate) outcome: Outcome,
    pub(crate) waited: Waited,
}

/// Presses of Ctrl-C, counted against the execution that was being waited on
/// when each was made. Shared between the host's wait and whoever relays them.
#[derive(Clone, Default)]
pub(crate) struct Presses(Arc<PressesInner>);

#[derive(Default)]
struct PressesInner {
    /// The execution being waited on, and how many times it has been pressed.
    /// One at a time: a call waits on one execution, and a turn runs its calls
    /// in order.
    current: Mutex<Option<(ExecId, u32)>>,
    changed: Notify,
}

/// What a press does, decided here beside [`settle`], which does it, so the
/// user is told what happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Press {
    /// The first on an execution: cancel it, and interrupt it too if its loop
    /// is not turning.
    Stop(ExecId),
    /// Any after that: stop waiting for it.
    GiveUp(ExecId),
}

/// Marks an execution as the one presses reach, until dropped.
pub(crate) struct Waiting {
    presses: Presses,
    id: ExecId,
}

impl Presses {
    /// Count a press against the execution being waited on, and say what it
    /// does; `None` if nothing is waited on.
    pub(crate) fn press(&self) -> Option<Press> {
        let press = {
            let mut current = self.current();
            let (id, count) = current.as_mut()?;
            *count += 1;
            if *count == 1 {
                Press::Stop(*id)
            } else {
                Press::GiveUp(*id)
            }
        };
        self.0.changed.notify_waiters();
        Some(press)
    }

    /// Direct presses at `id` until the returned guard drops.
    pub(crate) fn waiting_on(&self, id: ExecId) -> Waiting {
        *self.current() = Some((id, 0));
        Waiting {
            presses: self.clone(),
            id,
        }
    }

    /// Resolves once `id` has been pressed more than `seen` times.
    async fn pressed(&self, id: ExecId, seen: u32) {
        loop {
            // Enabled before the count is read, so a press between the two
            // still wakes it.
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if matches!(*self.current(), Some((current, count)) if current == id && count > seen) {
                return;
            }
            changed.await;
        }
    }

    fn current(&self) -> MutexGuard<'_, Option<(ExecId, u32)>> {
        self.0
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let mut current = self.presses.current();
        if matches!(*current, Some((id, _)) if id == self.id) {
            *current = None;
        }
    }
}

/// Wait for `execution`'s outcome, acting on presses and on what checks find
/// as described above. The outcome wins every race: whatever the host is in
/// the middle of when it arrives, it is what is returned.
pub(crate) async fn settle(
    interpreter: &Interpreter,
    mut execution: Execution,
    presses: &Presses,
    timings: &Timings,
) -> Settled {
    let id = execution.id();
    let mut waited = Waited::default();
    if !execution.queued() {
        let outcome = execution.outcome().await;
        if let Outcome::Refused { holder } = outcome
            && interpreter.abandoned() == Some(holder)
        {
            waited.holder = Some(rescue(interpreter, holder, timings).await);
        }
        return Settled { outcome, waited };
    }

    let mut probe = None;
    let mut handled = 0;
    let mut attempts = 0;
    let mut starved_said = false;
    let mut next_check = Instant::now() + timings.check_every;
    loop {
        // Biased, so a press already made wins over a check already due.
        let pressed = tokio::select! {
            biased;
            outcome = execution.outcome() => return Settled { outcome, waited },
            () = presses.pressed(id, handled) => true,
            () = sleep_until(next_check) => false,
        };
        if pressed && handled > 0 {
            // Asked again: stop waiting, after a last look for an outcome
            // that may be on its way.
            let outcome = match timeout(timings.last_look, execution.outcome()).await {
                Ok(outcome) => outcome,
                Err(_) => give_up(interpreter, execution, &mut waited, GaveUp::User),
            };
            return Settled { outcome, waited };
        }
        let wait = if pressed {
            handled += 1;
            waited.user_stopped = true;
            interpreter.cancel(id);
            timings.user_probe
        } else {
            timings.probe
        };
        let verdict = tokio::select! {
            biased;
            outcome = execution.outcome() => return Settled { outcome, waited },
            // A press is acted on now rather than once the check has run its
            // course. The check's probe stays in `probe`, to be waited on
            // again rather than sent again.
            () = presses.pressed(id, handled) => continue,
            verdict = check(interpreter, id, &mut probe, wait, timings.cpu) => verdict,
        };
        next_check = Instant::now() + timings.check_every;
        probe_failed(interpreter, id, verdict);
        match verdict {
            // Healthy, or at least not spinning: nothing is done on the host's
            // own account.
            Verdict::Turning | Verdict::Blocked if !pressed => attempts = 0,
            // The cancel lands when the loop next runs it, and the code may
            // still catch it; either way the reply says.
            Verdict::Turning => {}
            // A person asked, and a blocking call -- `subprocess.run`, say --
            // is where the execution is.
            Verdict::Blocked => {
                tracing::warn!(
                    "execution {id} is blocked in a call its cancel cannot reach; interrupting it"
                );
                interpreter.interrupt(id, false);
            }
            Verdict::Spinning if !pressed && attempts == ATTEMPTS => {
                tracing::warn!(
                    "execution {id} is still spinning after {ATTEMPTS} interrupts; no longer \
                     waiting for it"
                );
                let outcome = give_up(interpreter, execution, &mut waited, GaveUp::Runaway);
                return Settled { outcome, waited };
            }
            Verdict::Spinning => {
                tracing::warn!(
                    "execution {id}: the event loop stopped answering while a CPU stayed busy; \
                     interrupting the code spinning on it"
                );
                interpreter.interrupt(id, true);
                if !pressed {
                    attempts += 1;
                    waited.runaway_interrupted = true;
                    next_check = Instant::now() + timings.grace;
                }
            }
            Verdict::Starved => {
                if !starved_said {
                    starved_said = true;
                    tracing::warn!(
                        "execution {id}: the interpreter is not answering at all -- native code \
                         is holding it, and nothing can interrupt that. Waiting for it to return"
                    );
                }
            }
        }
    }
}

/// Check `holder`, which holds the slot with nobody waiting for it, and
/// interrupt it if it is spinning. What it reports, it reports as a late
/// result.
async fn rescue(interpreter: &Interpreter, holder: ExecId, timings: &Timings) -> Verdict {
    let verdict = check(interpreter, holder, &mut None, timings.probe, timings.cpu).await;
    probe_failed(interpreter, holder, verdict);
    if verdict == Verdict::Spinning {
        tracing::warn!(
            "execution {holder}, which nobody is waiting for, is spinning; interrupting it"
        );
        interpreter.interrupt(holder, true);
    }
    verdict
}

/// Stop waiting for `execution`, for `why`: say so in `waited`, and record it.
fn give_up(
    interpreter: &Interpreter,
    execution: Execution,
    waited: &mut Waited,
    why: GaveUp,
) -> Outcome {
    waited.gave_up = Some(why);
    interpreter.events().emit(Event::ExecAbandoned {
        execid: execution.id(),
        why: match why {
            GaveUp::User => "user",
            GaveUp::Runaway => "runaway",
        },
    });
    execution.stop_waiting()
}

/// Record a check of `id`'s loop that found it not turning.
fn probe_failed(interpreter: &Interpreter, id: ExecId, verdict: Verdict) {
    if verdict != Verdict::Turning {
        interpreter.events().emit(Event::ExecProbeFailed {
            execid: id,
            verdict: verdict.name(),
        });
    }
}

/// A probe of the loop, sent and not yet answered.
type Probe<'a> = Pin<Box<dyn Future<Output = Result<Inventory, InterpreterError>> + Send + 'a>>;

/// Ask whether the loop answers within `wait`, and if not, whether its thread
/// used a CPU meanwhile.
///
/// A probe the loop has not answered is kept in `probe` for the next check
/// rather than sent again: it is still queued behind whatever holds the loop,
/// and a loop blocked for an hour would otherwise have a hundred of them to
/// answer when it returns. One answered before this check began is dropped
/// instead: it says the loop turned then, not that it turns now.
///
/// An answer is what the agent's namespace held while `id` ran, which is
/// recorded.
async fn check<'a>(
    interpreter: &'a Interpreter,
    id: ExecId,
    probe: &mut Option<Probe<'a>>,
    wait: Duration,
    cpu: Duration,
) -> Verdict {
    let reading = || async {
        match timeout(cpu, interpreter.cpu()).await {
            Err(_) => Err(Verdict::Starved),
            // Gone: the outcome is on its way, and there is nothing to do.
            Ok(Err(_)) => Err(Verdict::Turning),
            Ok(Ok(seconds)) => Ok((seconds, Instant::now())),
        }
    };
    let measured = async {
        if let Some(pending) = probe.as_mut()
            && timeout(Duration::ZERO, pending).await.is_ok()
        {
            *probe = None;
        }
        let before = reading().await?;
        let pending = probe.get_or_insert_with(|| Box::pin(interpreter.inventory()));
        if let Ok(answer) = timeout(wait, pending).await {
            *probe = None;
            if let Ok(inventory) = answer {
                observed(interpreter, id, &inventory);
            }
            return Ok(Verdict::Turning);
        }
        let after = reading().await?;
        Ok(match (before, after) {
            ((Some(used_before), at_before), (Some(used_after), at_after)) => {
                let wall = (at_after - at_before).as_secs_f64();
                if wall > 0.0 && (used_after - used_before) / wall >= BUSY {
                    Verdict::Spinning
                } else {
                    Verdict::Blocked
                }
            }
            // A clock the interpreter could not read says nothing either way,
            // and nothing is done on no evidence.
            _ => Verdict::Blocked,
        })
    };
    measured.await.unwrap_or_else(|verdict| verdict)
}

/// Record what a probe found the agent's namespace holding while `id` ran.
fn observed(interpreter: &Interpreter, id: ExecId, inventory: &Inventory) {
    let events = interpreter.events();
    if !events.is_on() {
        return;
    }
    events.emit(Event::InventoryObserved {
        execid: id,
        names: inventory
            .globals
            .iter()
            .map(|(name, kind)| Held { name, kind })
            .collect(),
        total: inventory.total,
        more: inventory.more,
    });
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
