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
//! A long execution keeps its slot for as long as it runs, and is watched for
//! all of that time ([`watch`], started with the slot by
//! [`Interpreter::submit`]), whether or not a call waits on it. Every
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
//! after [`Timings::grace`] rather than concluding. It stops waiting -- the
//! execution becomes [`Unresolved`](super::host::Unknown::Unresolved), keeping its slot -- only after
//! [`ATTEMPTS`] interrupts have each left the loop spinning.
//!
//! Stopping waiting is not stopping watching. The watch reports that it has
//! stopped waiting through [`Watched`], which [`settle`] reads, and goes on
//! checking every [`Timings::check_every`], interrupting whenever it finds the
//! loop spinning: with nobody waiting, an interrupt is all the host has to
//! keep a CPU from burning until the next submission. What it found last is
//! kept in the slot too, and a submission refused behind an abandoned
//! execution reads it at once ([`Outcome::Refused`]) rather than checking.
//!
//! The watch and a press each run checks of their own, so an interrupt sent
//! for one can land while the other is taking its reading, and a reading of
//! the thread's CPU clock across an interrupt is of neither the thread before
//! it nor the thread after. A spinner a press has interrupted into a `finally:`
//! that sleeps would still average as spinning, and the watch would interrupt
//! it out of its cleanup. So every interrupt sent for the execution is counted
//! in its [`Watched`], by whoever sends it ([`stop_abandoned`]'s too); a check
//! that finds the count moved while it ran is run again ([`examine`]), and the
//! interrupt the watch decides on is sent only if the count is still what the
//! check began at ([`Interpreter::interrupt_runaway`]). The check that comes
//! after reads the thread as the interrupt left it.
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
//! With no call waiting, a request reaches the execution left holding the
//! slot -- by a press that gave up, or by a round that was dropped -- as
//! often as it is asked ([`stop_abandoned`]): a cancel, and an interrupt
//! aimed at the execution's own code, with no check between. The check a
//! press runs is for what an aimed interrupt cannot do: widen to whatever
//! spins on the loop, and give the model a verdict. The aimed interrupt
//! itself needs no evidence to be safe, since the interpreter declines it
//! everywhere but in that execution's own code -- with the one exception
//! [`Interpreter::interrupt`] records: for an execution that has not
//! started, it lands in whatever keeps it from starting, which may be a
//! task an earlier execution left running. Nobody is waiting, so there is
//! no giving up.
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

use tokio::sync::{Notify, watch};
use tokio::time::{Instant, sleep_until, timeout};

use super::host::{ExecId, Execution, Interpreter, InterpreterError, Inventory, Outcome};
use crate::harness::event::{Abandoned, Held, Payload, ProbeVerdict};

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

/// How long each step of waiting takes, kept by the interpreter (see
/// [`Interpreter::timings`]). A test shortens them.
#[derive(Debug, Clone, Copy)]
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
}

/// What the host has found out and done about the execution holding the slot,
/// kept with the slot so that it outlasts whatever was waiting on it. Written
/// by the watch, and by a user's check; read by [`settle`], and by a refusal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watched {
    /// What the latest check found, or nothing before the first.
    pub(crate) verdict: Option<Verdict>,
    /// Whether the host has interrupted a runaway on its own account.
    pub(crate) interrupted: bool,
    /// How many interrupts have been sent for the execution, by anyone: the
    /// watch's, a press's, a stop with nobody waiting. A check begins at one
    /// count and is held to it (see [`Interpreter::found`]).
    pub(crate) interrupts: u32,
    /// Whether the host has stopped waiting: [`ATTEMPTS`] interrupts have each
    /// left the loop spinning.
    pub(crate) gave_up: bool,
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
    /// As the event log records a check that found the loop not turning, or
    /// `None` for one that found it turning.
    pub(super) fn failed(self) -> Option<ProbeVerdict> {
        match self {
            Verdict::Turning => None,
            Verdict::Blocked => Some(ProbeVerdict::Blocked),
            Verdict::Spinning => Some(ProbeVerdict::Spinning),
            Verdict::Starved => Some(ProbeVerdict::Starved),
        }
    }
}

/// What became of a check's verdict when it was recorded
/// ([`Interpreter::found`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    /// It is the execution's latest, and stands.
    Recorded,
    /// An interrupt was sent while the check ran, so a reading of the thread's
    /// CPU clock across it is of neither the thread before nor the thread
    /// after. Nothing was recorded.
    Stale,
    /// The execution has completed.
    Freed,
}

/// A verdict [`examine`] stands behind, and the interrupt count its check
/// began at.
#[derive(Debug, Clone, Copy)]
struct Examined {
    verdict: Verdict,
    seen: u32,
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

/// Wait for `execution`'s outcome, acting on presses, and on the watch's having
/// given up, as described above. The outcome wins every race: whatever the host
/// is in the middle of when it arrives, it is what is returned.
///
/// What the host does on its own account is the watch's, and is only read
/// here, from the execution's [`Watched`].
pub(crate) async fn settle(
    interpreter: &Interpreter,
    mut execution: Execution,
    presses: &Presses,
) -> Settled {
    let mut waited = Waited::default();
    if !execution.queued() {
        // Nothing ran, and the refusal is already here.
        let outcome = execution.outcome().await;
        return Settled { outcome, waited };
    }
    let mut watched = execution.watched();
    let outcome = wait(interpreter, execution, presses, &mut watched, &mut waited).await;
    waited.runaway_interrupted = watched.borrow().interrupted;
    Settled { outcome, waited }
}

/// [`settle`]'s wait, noting in `waited` what the user did and why the host
/// stopped waiting.
async fn wait(
    interpreter: &Interpreter,
    mut execution: Execution,
    presses: &Presses,
    watched: &mut watch::Receiver<Watched>,
    waited: &mut Waited,
) -> Outcome {
    let id = execution.id();
    let timings = interpreter.timings();
    let mut handled = 0;
    loop {
        // Biased, so a press already made wins over the host's giving up.
        let pressed = tokio::select! {
            biased;
            outcome = execution.outcome() => return outcome,
            () = presses.pressed(id, handled) => true,
            () = given_up(watched) => false,
        };
        if !pressed {
            return give_up(interpreter, execution, waited, GaveUp::Runaway);
        }
        if handled > 0 {
            // Asked again: stop waiting, after a last look for an outcome that
            // may be on its way.
            return match timeout(timings.last_look, execution.outcome()).await {
                Ok(outcome) => outcome,
                Err(_) => give_up(interpreter, execution, waited, GaveUp::User),
            };
        }
        handled += 1;
        waited.user_stopped = true;
        interpreter.cancel(id);
        // This check's own probe. One the watch has out is that task's to
        // wait on again, so a press with the loop blocked sends one more.
        let mut probe = None;
        let examined = tokio::select! {
            biased;
            outcome = execution.outcome() => return outcome,
            // A press is acted on now rather than once the check has run its
            // course.
            () = presses.pressed(id, handled) => continue,
            examined = examine(interpreter, id, &mut probe, timings.user_probe, timings.cpu) => examined,
        };
        let Some(Examined { verdict, .. }) = examined else {
            // The slot is free, so the outcome is here.
            return execution.outcome().await;
        };
        match verdict {
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
            // Not the host's own interrupt, so it is no attempt, and the call
            // was not interrupted as a runaway.
            Verdict::Spinning => {
                tracing::warn!(
                    "execution {id}: the event loop stopped answering while a CPU stayed busy; \
                     interrupting the code spinning on it"
                );
                interpreter.interrupt(id, true);
            }
            Verdict::Starved => starved(id),
        }
    }
}

/// Watch `id`, which holds the slot, for as long as it does: check on its loop
/// every [`Timings::check_every`], and interrupt a runaway, as described above.
/// Nothing here depends on a call waiting, so it goes on for an execution
/// nobody waits for, and ends when the slot is free.
///
/// Holds a [`downgrade`](Interpreter::downgrade)d handle, which does not keep
/// the interpreter's stdin open: dropping every other handle ends the
/// interpreter, the slot with it, and so this.
pub(crate) async fn watch(
    interpreter: Interpreter,
    id: ExecId,
    timings: Timings,
    mut told: watch::Receiver<Watched>,
) {
    let mut probe = None;
    let mut attempts = 0;
    let mut gave_up = false;
    let mut starved_said = false;
    let mut next_check = Instant::now() + timings.check_every;
    loop {
        tokio::select! {
            biased;
            () = freed(&mut told) => return,
            () = sleep_until(next_check) => {}
        }
        let examined = tokio::select! {
            biased;
            () = freed(&mut told) => return,
            examined = examine(&interpreter, id, &mut probe, timings.probe, timings.cpu) => examined,
        };
        let Some(Examined { verdict, seen }) = examined else {
            return;
        };
        next_check = Instant::now() + timings.check_every;
        match verdict {
            // Healthy, or at least not spinning: nothing is done on the host's
            // own account.
            Verdict::Turning | Verdict::Blocked => attempts = 0,
            // Every interrupt left it spinning. Whoever waits stops waiting,
            // and nothing is sent at this check: the next one is the first
            // that is for an execution nobody waits for.
            Verdict::Spinning if !gave_up && attempts == ATTEMPTS => {
                tracing::warn!(
                    "execution {id} is still spinning after {ATTEMPTS} interrupts; no longer \
                     waiting for it"
                );
                gave_up = true;
                interpreter.note(id, |watched| watched.gave_up = true);
            }
            Verdict::Spinning => {
                tracing::warn!(
                    "execution {id}: the event loop stopped answering while a CPU stayed busy; \
                     interrupting the code spinning on it"
                );
                if interpreter.interrupt_runaway(id, seen) {
                    attempts += 1;
                    if !gave_up {
                        next_check = Instant::now() + timings.grace;
                    }
                } else {
                    // Another interrupt came between the check and this one,
                    // and may have done what this was to: look again at once.
                    next_check = Instant::now();
                }
            }
            Verdict::Starved => {
                if !starved_said {
                    starved_said = true;
                    starved(id);
                }
            }
        }
    }
}

/// [`check`] `id`'s loop, and record what it found.
///
/// A check is of the loop as it was while the check ran, and a verdict drawn
/// from the thread's CPU clock is only that if nothing sent the thread an
/// interrupt meanwhile. The watch and a press run checks of their own, side by
/// side, so one's interrupt can land in the middle of the other's reading. A
/// check that an interrupt has made stale is run again, at once, with the probe
/// it left kept; the reading it takes then begins after that interrupt. Without
/// this, a spinner a press has interrupted into a `finally:` that sleeps for ten
/// seconds still reads as spinning to a watch that began before it, and is
/// interrupted again, out of the cleanup.
///
/// The decision made on the verdict is held to the same count, which is why it
/// is returned: [`Interpreter::interrupt_runaway`] sends only if it is still
/// what the check began at.
///
/// `None` once the slot is free.
async fn examine<'a>(
    interpreter: &'a Interpreter,
    id: ExecId,
    probe: &mut Option<Probe<'a>>,
    wait: Duration,
    cpu: Duration,
) -> Option<Examined> {
    loop {
        let seen = interpreter.interrupts(id)?;
        let verdict = check(interpreter, id, probe, wait, cpu).await;
        match interpreter.found(id, seen, verdict) {
            Found::Recorded => return Some(Examined { verdict, seen }),
            Found::Stale => {}
            Found::Freed => return None,
        }
    }
}

/// Resolves once the slot is free: its record, whose sender goes with it, will
/// not change again.
async fn freed(told: &mut watch::Receiver<Watched>) {
    while told.changed().await.is_ok() {}
}

/// Resolves once the host has given up on the execution; never if its slot is
/// freed first, when its outcome is already here.
async fn given_up(told: &mut watch::Receiver<Watched>) {
    loop {
        if told.borrow_and_update().gave_up {
            return;
        }
        if told.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

fn starved(id: ExecId) {
    tracing::warn!(
        "execution {id}: the interpreter is not answering at all -- native code is holding it, \
         and nothing can interrupt that. Waiting for it to return"
    );
}

/// Stop the execution holding the slot with nobody waiting for it -- recorded
/// [`Unresolved`](super::host::Unknown::Unresolved), or whose [`Execution`]
/// was dropped -- and say which; `None`, sending nothing, when there is none.
/// The user's request, which the host never makes on its own: what [`watch`]
/// does on its own account never cancels.
///
/// Both remedies go, with no check between (see the module docs): the cancel
/// for an execution suspended on an await, and an interrupt aimed at its own
/// code for one blocked in a call of its own or spinning in its own body.
/// Where each lands, and what a reply racing them means, is
/// [`Interpreter::cancel`]'s and [`Interpreter::interrupt`]'s.
pub(crate) fn stop_abandoned(interpreter: &Interpreter) -> Option<ExecId> {
    let holder = interpreter.abandoned()?;
    interpreter.cancel(holder);
    interpreter.interrupt(holder, false);
    Some(holder)
}

/// Stop waiting for `execution`, for `why`: say so in `waited`, and record it.
fn give_up(
    interpreter: &Interpreter,
    execution: Execution,
    waited: &mut Waited,
    why: GaveUp,
) -> Outcome {
    waited.gave_up = Some(why);
    interpreter.events().emit(Payload::ExecAbandoned {
        execid: execution.id().public(),
        why: match why {
            GaveUp::User => Abandoned::User,
            GaveUp::Runaway => Abandoned::Runaway,
        },
    });
    execution.stop_waiting()
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
    interpreter
        .events()
        .emit_with(|| Payload::InventoryObserved {
            execid: id.public(),
            names: inventory
                .globals
                .iter()
                .map(|(name, kind)| Held {
                    name: name.clone(),
                    type_name: kind.clone(),
                })
                .collect(),
            total: inventory.total as u64,
            more: inventory.more as u64,
        });
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
