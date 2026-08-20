//! Ownership for cleanup work that outlives the future which owed it.
//!
//! `Drop` cannot await, so a destructor cannot run the `podman rm -f` or
//! `buildah rm` that a cancelled future left owing. What it *can* do is spawn
//! a detached process and hand the reap to someone else. That is what this
//! module is: [`detach_cleanup`] launches the cleanup command with nulled
//! stdio and moves its handle onto a thread that blocks in `wait`.
//!
//! Two properties come from being an ordinary OS process rather than a task:
//!
//! - It needs no tokio runtime, so it is callable from `Drop` and from a
//!   panic hook -- the two places that have no runtime to lean on.
//! - It runs to completion even if outrig's runtime is torn down, or outrig
//!   itself exits, immediately afterwards. A spawned task would be cancelled
//!   by either.
//!
//! Reaping is what this adds over a bare fire-and-forget spawn. Without it
//! every cleanup leaves a zombie for the life of the process, which is what
//! the pre-0.2.0 `spawn_detached_rm` did.
//!
//! There is **one** reaper thread for the process, and it polls with
//! `try_wait` rather than blocking on one child at a time. Both halves matter:
//! a thread per cleanup would let a burst of cancellations -- or a single
//! interceptor shutdown with many attachments -- spend threads, pids and stack
//! reservations in proportion to how many cleanups are in flight, and a queue
//! of blocking waits would let one wedged `podman rm` delay the reaping of
//! every cleanup behind it. Polling costs nothing while idle, because the
//! thread blocks on the channel whenever it is holding no children.
//!
//! See [`crate::process`] for the other half of the story -- the child
//! processes outrig itself is waiting on, and what dropping their future
//! guarantees.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use crate::process::Cmd;

/// How often the reaper re-checks the children it is holding. Only paid while
/// it is holding some; it blocks on the channel otherwise.
const REAP_POLL: Duration = Duration::from_millis(50);

/// How long a cleanup gets before the reaper concludes it is wedged and kills
/// it.
///
/// This is a bound on the *population*, not an opinion about how long a
/// removal should take: without it, cleanups that never return accumulate as
/// live processes for the life of the process, and a caller that cancels
/// repeatedly against a wedged engine would spend pids and handles without
/// limit. A healthy `podman rm -f` finishes in well under a second even when
/// it has to stop the container first, so a minute is far past "slow" and
/// squarely in "will not finish".
const WEDGED_CLEANUP: Duration = Duration::from_secs(60);

/// Whether a cleanup command may be issued a second time.
///
/// A retry re-resolves whatever the command names, at a moment later than the
/// attempt that failed -- so the question is not "did it fail" but "can this
/// argv still mean the same thing". For a per-attempt label or a nonce-bearing
/// name, yes: nothing else on the machine can ever become that. For a bare
/// container name or a pid, no: the engine and the kernel hand both out again,
/// and a retry landing after one moved on would act on whoever holds it now --
/// deleting a replacement container, or entering a replacement namespace and
/// dropping *its* nft table. That is a worse outcome than the leak the retry
/// exists to prevent, so the caller states which kind it has rather than this
/// module guessing from an argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reissue {
    /// The command selects something only this attempt can be.
    Safe,
    /// The command selects by an identity that can be reused. Issued once.
    Once,
}

/// What this module has done to the attempt in flight.
///
/// Per attempt, never per obligation: a retry is a different process, so it
/// gets its own wedge deadline and inherits nothing that was aimed at the
/// attempt before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kill {
    /// Nothing yet -- this attempt's wedge deadline is still ahead of it.
    Untried,
    /// Delivered, so an ending that carries no exit code is this module's.
    Sent,
    /// Attempted and refused. Nothing reached the attempt, so however it ends
    /// is not this module's doing; recorded rather than left `Untried` so the
    /// same failing kill is not re-issued and re-logged on every poll.
    Failed,
}

/// Whether a finished attempt was ended by this module's wedge kill, rather
/// than merely having been signalled while one was in flight.
///
/// A delivered kill alone is not proof of causation, and neither half of the
/// question can be dropped.
///
/// `Child::kill` returning `Ok` says a `SIGKILL` was *queued*, not that it is
/// what the process died of. A cleanup can reach its own ending in the window
/// between the `try_wait` that reported it running and the `kill` that follows,
/// and the kill then succeeds against a process that is already a zombie -- so
/// a real non-zero exit would be filed as "we stopped it" and lose the retry it
/// had coming. Something else can win the race too: these cleanups inherit
/// outrig's process group, so a Ctrl-C or an operator's `SIGTERM` reaches them,
/// and either can be the signal `wait` collects after this module's `SIGKILL`
/// went out.
///
/// So the status has to match what was sent, not merely be a signal. `SIGKILL`
/// after a delivered `SIGKILL` is this module's doing; an exit code means the
/// command reached its own ending whatever happened afterwards, and any other
/// signal means something else ended it and the outcome is unknown -- which is
/// the case a replayable cleanup exists for.
///
/// The one it cannot separate is the OOM killer, which sends `SIGKILL` too.
/// Unfixable from an exit status, and the conservative way round: a machine
/// that is out of memory is not one to spend three more processes on.
fn ended_by_our_kill(kill: Kill, status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    kill == Kill::Sent && status.signal() == Some(nix::sys::signal::Signal::SIGKILL as i32)
}

/// Retries a failed cleanup gets before the obligation is abandoned.
///
/// A cleanup that exits non-zero has *not* discharged its obligation, and
/// nothing else will notice: the guard that owed it is gone, there is no
/// caller to report to, and no sweep comes later. Dropping it on the first
/// non-zero exit means a container survives a moment of engine or storage
/// contention permanently, which is the outcome this whole module exists to
/// prevent.
///
/// Finite, because the other failure mode is a removal that can never succeed
/// -- a name that no longer exists, a filter that matches nothing on an engine
/// that reports it as an error -- and retrying that forever would spend
/// processes on it forever. Two retries covers a transient lock; a third
/// attempt failing says the answer is not going to change.
const CLEANUP_RETRIES: u8 = 2;

/// Delay before the first retry; doubled for each one after it.
///
/// Long enough that a retry is not simply the same contended instant again,
/// short enough that the whole sequence is over well inside a session
/// teardown.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// Most *arrivals* the reaper accepts in one pass before it scans.
///
/// Draining until the channel reports empty reads well and starves under
/// sustained ingress: a producer that keeps the queue non-empty would keep the
/// reaper accepting forever, and neither the exit reaping nor the deadline
/// above would ever run. A cap makes each pass finite regardless of arrival
/// rate, and anything still queued is taken on the next one.
///
/// Counted per pass, and deliberately not against the number of cleanups being
/// held: a cap on the population would stop bulk admission precisely when
/// there is a backlog, leaving one arrival per scan.
const REAP_BATCH: usize = 64;

/// Run `cmd` as a detached cleanup process and take responsibility for
/// reaping it.
///
/// Synchronous and runtime-free: safe from `Drop` and from a panic hook. The
/// exit status is unavailable by construction -- nobody is left to receive it
/// -- so this is for obligations whose failure has nowhere to go. A caller
/// that can await and wants the status should run the command through
/// [`crate::process`] instead.
pub(crate) fn detach_cleanup(cmd: Cmd, reissue: Reissue) {
    let program = cmd.program;
    let waiting = match spawn_cleanup(&cmd) {
        // Timestamped at spawn, not at dequeue: the deadline is about how long
        // the command has been running, and time spent queued is time running.
        Ok(child) => Wait::running(cmd, child, reissue),
        // It never started. A binary that is missing or not executable will
        // still be missing in 250 ms, but a process table that was briefly
        // full will not -- and dropping the obligation there loses a resource
        // to a moment of pressure, which is exactly when cleanups arrive in
        // bulk.
        Err(e) => match Wait::pending(cmd, reissue, &e) {
            Some(waiting) => waiting,
            None => return,
        },
    };

    // Nobody took it. Reaching here takes running out of threads for the whole
    // process -- the reaper is created once, and its channel is unbounded.
    let unqueued = match reaper() {
        Some(reaper) => match reaper.send(waiting) {
            Ok(()) => return,
            Err(std::sync::mpsc::SendError(waiting)) => waiting,
        },
        None => waiting,
    };

    if unqueued.child.is_some() {
        // The cleanup is running; only its reap is lost. Deliberately left
        // alone rather than killed: the point of this module is that the
        // cleanup *happens*, and a zombie is the smaller loss than a
        // container that stays.
        tracing::debug!(
            target: "outrig::supervise",
            program,
            "no reaper available; cleanup runs unreaped"
        );
        return;
    }

    // Nothing is running at all: the first attempt hit a transient spawn
    // failure and the retry it was queued for has nowhere to happen now. One
    // more attempt, inline and unreaped, because `Drop` cannot wait out a
    // backoff and an unreaped cleanup still beats no cleanup. If even that
    // fails, the obligation is genuinely lost -- say so plainly rather than
    // logging the running-cleanup line, which would read as though something
    // were still in flight.
    match spawn_cleanup(&unqueued.cmd) {
        Ok(_) => tracing::debug!(
            target: "outrig::supervise",
            program,
            "no reaper available; retried cleanup runs unreaped"
        ),
        Err(_) => tracing::debug!(
            target: "outrig::supervise",
            program,
            "cleanup could not be started and could not be queued; the \
             resource is abandoned"
        ),
    }
}

/// Start one attempt at `cmd`, with stdio nulled.
fn spawn_cleanup(cmd: &Cmd) -> std::io::Result<Child> {
    Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .inspect_err(|e| {
            tracing::debug!(
                target: "outrig::supervise",
                program = cmd.program,
                error = %e,
                "cleanup command failed to spawn"
            );
        })
}

/// Whether a failure to spawn could come out differently in a moment.
///
/// `EAGAIN` and `ENOMEM` are the fan-out cases -- a full process table, a
/// memory ceiling -- and they pass. A binary that is missing or not executable
/// will not become either, so retrying that only spends attempts.
fn spawn_failure_is_transient(e: &std::io::Error) -> bool {
    !matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    )
}

/// The process-wide reaper, started on first use.
///
/// Only a *successful* start is remembered. Failing to spawn a thread is a
/// symptom of momentary resource pressure -- which is exactly when cleanups
/// pile up -- so caching that failure would disable reaping for the rest of
/// the process, permanently, on the strength of one bad instant. Each caller
/// retries instead.
fn reaper() -> Option<Sender<Wait>> {
    static REAPER: Mutex<Option<Sender<Wait>>> = Mutex::new(None);

    let mut started = REAPER.lock().ok()?;
    if let Some(tx) = started.as_ref() {
        return Some(tx.clone());
    }

    let (tx, rx) = channel::<Wait>();
    std::thread::Builder::new()
        .name("outrig-reap".to_string())
        .spawn(move || reap_loop(rx))
        .ok()?;
    *started = Some(tx.clone());
    Some(tx)
}

/// Hold every handed-over child until it exits, killing any that wedges.
///
/// `try_wait` never blocks, so a cleanup that hangs costs one entry in a `Vec`
/// and delays nothing else -- the reason this polls rather than waiting on one
/// child at a time. The thread parks on `recv` whenever it holds nothing, so
/// an idle process does not poll at all.
fn reap_loop(rx: Receiver<Wait>) {
    let mut waiting: Vec<Wait> = Vec::new();
    while admit_pass(&rx, &mut waiting) {
        waiting.retain_mut(Wait::still_owed);
    }
}

/// Admit one pass's worth of arrivals into `held`. Returns whether the reaper
/// should keep going.
///
/// One receive opens the pass -- blocking while `held` is empty, since there is
/// nothing to poll for -- and whatever is already queued follows it, so a burst
/// costs one scan rather than one per arrival. That opening receive spends
/// budget like any other arrival, so a pass never takes more than
/// [`REAP_BATCH`] in total.
///
/// The budget counts **arrivals**, and how many cleanups are already held is no
/// part of it: a budget against the held population would stop admitting in
/// bulk exactly when there is a backlog, leaving one arrival per scan -- the
/// quadratic the batching exists to avoid.
///
/// A disconnected channel ends the loop only while nothing is held: children
/// already in hand are still owed their reaps, and finishing them needs no
/// further arrival.
///
/// Generic over the item so the reaper's own bookkeeping -- not merely an inner
/// helper of it -- is exercisable against a plain channel; `reap_loop` is the
/// only caller that passes real children.
fn admit_pass<T>(rx: &Receiver<T>, held: &mut Vec<T>) -> bool {
    // What is left of the pass's allowance. The receive that opens it draws on
    // the same one as the arrivals that follow.
    let mut budget = REAP_BATCH;

    if held.is_empty() {
        match rx.recv() {
            Ok(item) => {
                held.push(item);
                budget -= 1;
            }
            // Unreachable while a sender is cached, but returning is the only
            // non-spinning answer if it ever is.
            Err(_) => return false,
        }
    } else if let Ok(item) = rx.recv_timeout(REAP_POLL) {
        held.push(item);
        budget -= 1;
    }

    // `take` stops without consuming a further arrival, so whatever the budget
    // does not cover stays queued for the next pass.
    held.extend(rx.try_iter().take(budget));
    true
}

/// One cleanup obligation: the attempt running now, and what is left to try.
struct Wait {
    /// Kept so a failed attempt can be replayed. Cheap -- a program name and
    /// its argv -- and safe to replay only when `reissue` says so.
    cmd: Cmd,
    /// The attempt in flight, or `None` between attempts.
    child: Option<Child>,
    /// When the attempt in flight began. Reset per attempt, since the wedged
    /// deadline is about one command rather than the obligation.
    since: Instant,
    /// What this module has done to the attempt in flight. Reset alongside
    /// `since`, for the same reason: it is a fact about one command.
    kill: Kill,
    reissue: Reissue,
    /// Attempts left after this one.
    retries_left: u8,
    /// When the next attempt may start, while `child` is `None`.
    retry_at: Option<Instant>,
}

impl Wait {
    /// An obligation whose first attempt is already running.
    fn running(cmd: Cmd, child: Child, reissue: Reissue) -> Self {
        Self {
            cmd,
            child: Some(child),
            since: Instant::now(),
            kill: Kill::Untried,
            reissue,
            retries_left: Self::budget(reissue),
            retry_at: None,
        }
    }

    /// An obligation whose first attempt could not be started, for a reason
    /// that may not hold in a moment. `None` when there is nothing worth
    /// queueing -- a one-shot cleanup, a budget of zero, or a permanent
    /// failure like a missing binary.
    fn pending(cmd: Cmd, reissue: Reissue, e: &std::io::Error) -> Option<Self> {
        let retries_left = Self::budget(reissue);
        if retries_left == 0 || !spawn_failure_is_transient(e) {
            return None;
        }
        Some(Self {
            cmd,
            child: None,
            since: Instant::now(),
            kill: Kill::Untried,
            reissue,
            retries_left: retries_left - 1,
            retry_at: Some(Instant::now() + RETRY_BACKOFF),
        })
    }

    fn budget(reissue: Reissue) -> u8 {
        match reissue {
            Reissue::Safe => CLEANUP_RETRIES,
            Reissue::Once => 0,
        }
    }

    /// Whether this obligation still needs holding.
    ///
    /// Three states end it: the cleanup succeeded, it failed with nothing left
    /// to try, or it cannot be reaped at all. A non-zero *exit* with retries
    /// left is not an ending -- the resource is still there, and nothing else
    /// is coming for it.
    fn still_owed(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return self.start_retry();
        };
        match child.try_wait() {
            // Still running: kill it if it has had too long, and keep holding
            // it either way so the kill is collected rather than left a
            // zombie.
            Ok(None) => {
                if self.kill == Kill::Untried && self.since.elapsed() >= WEDGED_CLEANUP {
                    match child.kill() {
                        Ok(()) => {
                            self.kill = Kill::Sent;
                            tracing::debug!(
                                target: "outrig::supervise",
                                pid = child.id(),
                                "cleanup command did not finish; killed"
                            );
                        }
                        // Nothing was delivered, so nothing was done to this
                        // attempt and no ending of it is ours. Recorded, so the
                        // kill is neither re-issued nor re-logged on every poll
                        // from here on.
                        Err(e) => {
                            self.kill = Kill::Failed;
                            tracing::debug!(
                                target: "outrig::supervise",
                                pid = child.id(),
                                error = %e,
                                "cleanup command did not finish and could not be killed"
                            );
                        }
                    }
                }
                true
            }
            Ok(Some(status)) if status.success() => false,
            // Killed by this module for wedging, and so terminal whatever the
            // budget says: the next attempt would be the same command against
            // the same engine that just held it for `WEDGED_CLEANUP`, and the
            // deadline exists to stop spending processes on that.
            Ok(Some(status)) if ended_by_our_kill(self.kill, &status) => {
                tracing::debug!(
                    target: "outrig::supervise",
                    program = self.cmd.program,
                    "cleanup command was killed for wedging; the resource is abandoned"
                );
                false
            }
            Ok(Some(_)) if self.retries_left == 0 => {
                tracing::debug!(
                    target: "outrig::supervise",
                    program = self.cmd.program,
                    reissue = ?self.reissue,
                    "cleanup command failed with no attempts left; the \
                     resource is abandoned"
                );
                false
            }
            // Everything else that did not exit zero, signals included. A
            // signal leaves the engine-side outcome *unknown* rather than
            // known-failed -- an OOM kill or an operator's SIGKILL says
            // nothing about whether the container went away -- and unknown is
            // the case a replayable cleanup is for. `Reissue::Once` never
            // reaches here: its budget is zero, so the arm above took it.
            Ok(Some(_)) => {
                let backoff = RETRY_BACKOFF * (1 << (CLEANUP_RETRIES - self.retries_left) as u32);
                self.retries_left -= 1;
                self.child = None;
                self.retry_at = Some(Instant::now() + backoff);
                true
            }
            // `try_wait` itself failing means this child can never be
            // collected, so holding it achieves nothing.
            Err(_) => false,
        }
    }

    /// Start the next attempt once its backoff has elapsed. Returns whether
    /// this is still owed.
    fn start_retry(&mut self) -> bool {
        match self.retry_at {
            Some(at) if Instant::now() < at => true,
            _ => match spawn_cleanup(&self.cmd) {
                Ok(child) => {
                    tracing::debug!(
                        target: "outrig::supervise",
                        program = self.cmd.program,
                        pid = child.id(),
                        "retrying a cleanup that did not discharge"
                    );
                    self.child = Some(child);
                    self.since = Instant::now();
                    self.kill = Kill::Untried;
                    self.retry_at = None;
                    true
                }
                // It could not be started, which `spawn_cleanup` recorded.
                // The same classification as the first attempt: a full process
                // table clears, a missing binary does not, and there is no
                // reason for a retry's spawn failure to end an obligation that
                // an initial one would have kept.
                Err(e) => {
                    if self.retries_left == 0 || !spawn_failure_is_transient(&e) {
                        return false;
                    }
                    let backoff =
                        RETRY_BACKOFF * (1 << (CLEANUP_RETRIES - self.retries_left) as u32);
                    self.retries_left -= 1;
                    self.retry_at = Some(Instant::now() + backoff);
                    true
                }
            },
        }
    }
}

/// An obligation to remove something, discharged through [`detach_cleanup`]
/// if it is still owed when this is dropped.
///
/// The shape every engine-side resource outrig names *before* the command
/// that creates it wants: arm the guard first, and no instant exists at which
/// the engine could hold the resource with nothing responsible for it. The
/// ordinary paths still remove these themselves, awaited and in order; the
/// guard covers the path that never gets there.
///
/// Removing something twice has to be harmless, because on most paths it will
/// be: `buildah rmi` of a tag that is already gone is a no-op.
pub(crate) struct CleanupGuard(Option<(Cmd, Reissue)>);

impl CleanupGuard {
    /// Take responsibility for running `remove` unless released first.
    ///
    /// `reissue` is the caller's statement about what `remove` selects; see
    /// [`Reissue`], and prefer [`Reissue::Once`] when in doubt, since the cost
    /// of being wrong the other way is someone else's resource.
    pub(crate) fn arm(remove: Cmd, reissue: Reissue) -> Self {
        Self(Some((remove, reissue)))
    }

    /// The resource is gone by other means, or an owner that will remove it
    /// now exists. Release **after** the awaited cleanup, never before: a
    /// cancellation landing inside that cleanup is exactly the case the guard
    /// is for.
    pub(crate) fn release(mut self) {
        self.0 = None;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if let Some((cmd, reissue)) = self.0.take() {
            detach_cleanup(cmd, reissue);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait for `path` to exist, or fail. The cleanups here are real
    /// processes, so what a test waits on is a side effect on disk.
    fn wait_for(path: &std::path::Path, within: Duration, what: &str) {
        let deadline = Instant::now() + within;
        while !path.exists() {
            assert!(Instant::now() < deadline, "{what} within {within:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A cleanup that exits non-zero has not discharged its obligation, so it
    /// is tried again. Nothing else is coming for the resource -- the guard
    /// that owed the removal is already gone -- which is why a transient
    /// engine failure must not be the end of it.
    ///
    /// The script fails once and succeeds after, so the success marker can
    /// only appear if a *second* attempt ran.
    #[test]
    fn a_cleanup_that_fails_once_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tried = dir.path().join("tried");
        let ran = dir.path().join("ran");

        detach_cleanup(
            Cmd::new("/bin/sh").arg("-c").arg(format!(
                "if [ -e {tried} ]; then touch {ran}; else touch {tried}; exit 1; fi",
                tried = tried.display(),
                ran = ran.display()
            )),
            Reissue::Safe,
        );

        wait_for(
            &tried,
            Duration::from_secs(5),
            "the first attempt never ran",
        );
        wait_for(
            &ran,
            Duration::from_secs(5),
            "the failure was never retried",
        );
    }

    /// The wedge classification turns on the exit status and on a kill this
    /// module actually delivered, not on one merely having been attempted.
    ///
    /// Unit-tested rather than raced for: the misfiling needs a cleanup to exit
    /// in the microseconds between `try_wait` and `kill`, at the far end of a
    /// 60-second deadline, which is not a thing a test can arrange. What can be
    /// pinned is the rule that decides it.
    #[test]
    fn a_kill_that_lost_the_race_does_not_count_as_ours() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;

        // Killed by SIGKILL with our kill delivered: ours.
        assert!(ended_by_our_kill(Kill::Sent, &ExitStatus::from_raw(9)));
        // Exited 1 with our kill delivered: the command reached its own ending
        // first, so this is a failure with a retry coming, not a termination
        // this module caused.
        assert!(!ended_by_our_kill(
            Kill::Sent,
            &ExitStatus::from_raw(1 << 8)
        ));
        // No kill attempted: never ours, however it ended.
        assert!(!ended_by_our_kill(Kill::Untried, &ExitStatus::from_raw(9)));
        assert!(!ended_by_our_kill(
            Kill::Untried,
            &ExitStatus::from_raw(1 << 8)
        ));
        // A kill that was refused reached nothing, so the signal that ended
        // this attempt came from somewhere else and the retry stands.
        assert!(!ended_by_our_kill(Kill::Failed, &ExitStatus::from_raw(9)));
        // Delivered a SIGKILL, collected something else. These cleanups sit in
        // outrig's own process group, so a Ctrl-C or an operator's SIGTERM
        // reaches them and can win the race -- and then the engine-side outcome
        // is unknown rather than known-abandoned, which is what the retry is
        // for. `kill` returning `Ok` only queued a signal; it did not say which
        // one arrived first.
        assert!(!ended_by_our_kill(
            Kill::Sent,
            &ExitStatus::from_raw(nix::sys::signal::Signal::SIGTERM as i32)
        ));
        assert!(!ended_by_our_kill(
            Kill::Sent,
            &ExitStatus::from_raw(nix::sys::signal::Signal::SIGINT as i32)
        ));
    }

    /// A retry is a new attempt and begins with nothing having been done to it.
    ///
    /// Carrying the kill record across attempts left the second one with no
    /// wedge deadline -- the deadline only fires while nothing has been tried
    /// yet -- and filed whatever signal ended it as this module's doing, which
    /// is terminal for the obligation.
    #[test]
    fn a_retry_starts_a_fresh_attempt() {
        let mut owed = Wait {
            cmd: Cmd::new("/bin/sh").arg("-c").arg("exit 0"),
            child: None,
            since: Instant::now(),
            // As if the attempt before this one had been killed for wedging.
            kill: Kill::Sent,
            reissue: Reissue::Safe,
            retries_left: 1,
            retry_at: None,
        };

        assert!(owed.start_retry(), "a retry with budget left is still owed");
        assert_eq!(owed.kill, Kill::Untried);

        // Reaped here, since nothing handed this one to the reaper.
        owed.child
            .take()
            .expect("the retry started")
            .wait()
            .expect("the retry is reapable");
    }

    /// A cleanup that was *signalled* is retried too, when its selector is
    /// replayable.
    ///
    /// An exit code says the command ran and failed; a signal says nothing at
    /// all about whether the resource went away -- an OOM kill or an
    /// operator's SIGKILL leaves the engine in an unknown state, which is
    /// precisely what a replayable removal is for. The wedged case is the
    /// exception, and it is the one this module caused itself.
    ///
    /// The first attempt parks so the test can kill it; only a second attempt
    /// can leave the marker.
    #[test]
    fn a_signalled_cleanup_is_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tried = dir.path().join("tried");
        let ran = dir.path().join("ran");

        detach_cleanup(
            Cmd::new("/bin/sh").arg("-c").arg(format!(
                "if [ -e {ran}.attempted ]; then touch {ran}; else touch {ran}.attempted; \
                 echo $$ >> {tried}; exec sleep 3600; fi",
                ran = ran.display(),
                tried = tried.display()
            )),
            Reissue::Safe,
        );

        let pid = published_pid(&tried, "the first attempt");
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );

        wait_for(
            &ran,
            Duration::from_secs(5),
            "a signalled cleanup was never retried",
        );
    }

    /// The retries are finite. A removal that can never succeed -- a name that
    /// is already gone on an engine that calls that an error -- must not spend
    /// processes forever, so the obligation is abandoned once the answer stops
    /// changing.
    #[test]
    fn a_cleanup_that_always_fails_stops_after_its_retries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("attempts");

        detach_cleanup(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo x >> {}; exit 1", log.display())),
            Reissue::Safe,
        );

        let attempts = |log: &std::path::Path| {
            std::fs::read_to_string(log)
                .map(|t| t.lines().count())
                .unwrap_or(0)
        };
        let want = CLEANUP_RETRIES as usize + 1;

        // Every backoff, plus room for a loaded machine to run the shells.
        let deadline = Instant::now() + Duration::from_secs(10);
        while attempts(&log) < want {
            assert!(
                Instant::now() < deadline,
                "only {} of {want} attempts ran",
                attempts(&log)
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Past the backoff a fourth attempt would have used, so "no more" is
        // observed rather than assumed from the loop exiting.
        std::thread::sleep(RETRY_BACKOFF * 4 + Duration::from_millis(200));
        assert_eq!(
            attempts(&log),
            want,
            "the obligation should be abandoned after {want} attempts"
        );
    }

    /// The cleanup runs, and its own child is reaped rather than left a
    /// zombie -- the defect that distinguishes this from a bare detached
    /// spawn. Proven by side effect: the marker file only appears if the
    /// process ran, and the reaper thread only exits once `wait` returned.
    #[test]
    fn detached_cleanup_runs_and_is_reaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");

        detach_cleanup(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("touch {}", marker.display())),
            Reissue::Safe,
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.exists(), "detached cleanup command did not run");
    }

    /// An armed guard that is never released discharges its obligation.
    #[test]
    fn an_unreleased_guard_runs_its_cleanup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("swept");

        drop(CleanupGuard::arm(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("touch {}", marker.display())),
            Reissue::Safe,
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.exists(), "a dropped guard did not run its cleanup");
    }

    /// A released guard does not. Without this the guard could be a no-op in
    /// the other direction and the test above would still pass.
    #[test]
    fn a_released_guard_runs_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("swept");

        CleanupGuard::arm(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("touch {}", marker.display())),
            Reissue::Safe,
        )
        .release();

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!marker.exists(), "a released guard still ran its cleanup");
    }

    /// Kills the blocked cleanups however the test ends, so a failed assertion
    /// cannot leave processes behind for the next run to trip over.
    struct Blockers {
        pids: Vec<i32>,
        /// The marker files, so teardown can kill a generation this test never
        /// learned the pid of. Belt to `pids`' braces: a retry should not
        /// happen at all, and if one ever does it must not outlive the run.
        markers: Vec<std::path::PathBuf>,
        /// Pids confirmed to have left the process table. The number is the
        /// kernel's to reuse from that moment, so a retired pid is never
        /// signalled again -- including out of `markers`, which still record
        /// it.
        retired: Vec<i32>,
    }

    impl Blockers {
        fn kill_all(&self) {
            let recorded = self.markers.iter().flat_map(|m| generations(m));
            for pid in self.pids.iter().copied().chain(recorded) {
                if self.retired.contains(&pid) {
                    continue;
                }
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }

        /// Confirm every recorded pid has left the process table, retiring
        /// each one the moment it has.
        ///
        /// Retired one at a time rather than in a batch at the end: a pid is
        /// stale from the instant it is confirmed gone, and the test asserts
        /// -- and so can panic into `Drop` -- after that point. A pid this has
        /// not reached yet is still live and stays armed, including when the
        /// assertion in here is the one that fails.
        fn confirm_all_gone(&mut self) {
            for pid in self.pids.clone() {
                wait_until_gone(pid, "a released cleanup");
                self.retired.push(pid);
            }
        }
    }

    impl Drop for Blockers {
        fn drop(&mut self) {
            self.kill_all();
        }
    }

    fn wait_until_gone(pid: i32, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "{what} (pid {pid}) was still in the process table"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn published_pid(marker: &std::path::Path, what: &str) -> i32 {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(marker)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            assert!(std::time::Instant::now() < deadline, "{what} never started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Spawn a cleanup that blocks until killed, and return its pid. `exec` so
    /// the pid is the sleeper itself, and a bare sleep rather than a poll loop
    /// so that dozens of these cost dozens of idle processes rather than a
    /// fork storm.
    fn blocked_cleanup(marker: &std::path::Path) -> i32 {
        // `Once`, because this test ends its blockers by killing them, and a
        // kill is a case the supervisor replays for a `Safe` selector -- the
        // resource might still be there. Replaying here would only respawn a
        // sleeper the test never learns the pid of, which is how an earlier
        // cut of the retry leaked 72 processes per run. What the blockers are
        // for is the reaper's scheduling; `a_signalled_cleanup_is_retried`
        // covers the policy.
        //
        // The marker appends rather than overwrites, so it records every
        // generation this obligation started and not just the last -- which is
        // what makes the assertion at the end of the test able to see a
        // regression rather than infer one.
        detach_cleanup(
            Cmd::new("/bin/sh")
                .arg("-c")
                .arg(format!("echo $$ >> {}; exec sleep 3600", marker.display())),
            Reissue::Once,
        );
        published_pid(marker, "a blocked cleanup")
    }

    /// Every pid a marker file records, in the order they were started.
    fn generations(marker: &std::path::Path) -> Vec<i32> {
        std::fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    /// Cleanups that have not finished delay nothing else, and every finished
    /// one is reaped rather than left a zombie, with a held population well
    /// past `REAP_BATCH`.
    ///
    /// This is the property that a thread per cleanup, or a queue of blocking
    /// waits, would each fail differently: the first by spending a thread on
    /// every entry, the second by holding the fast cleanups behind the ones
    /// that never return.
    ///
    /// It does **not** cover the admission budget. Each blocker is awaited
    /// before the next is spawned, so the reaper gets to admit and scan in
    /// between and no queued burst ever forms -- the one-arrival-per-scan bug
    /// passes this happily. `admission_is_budgeted_by_arrivals_not_by_backlog`
    /// is what pins that, against a channel rather than a scheduler.
    ///
    /// `kill(pid, 0)` succeeds for a zombie, so its failure is the reap and not
    /// merely the exit.
    #[test]
    fn blocked_cleanups_do_not_delay_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");

        // Queued first, so a reaper that waits in order is stuck behind them.
        let markers: Vec<std::path::PathBuf> = (0..super::REAP_BATCH + 8)
            .map(|i| dir.path().join(format!("blocked.{i}")))
            .collect();
        let mut blockers = Blockers {
            pids: markers.iter().map(|m| blocked_cleanup(m)).collect(),
            markers: markers.clone(),
            retired: Vec::new(),
        };

        for i in 0..12 {
            let marker = dir.path().join(format!("fast.{i}"));
            detach_cleanup(
                Cmd::new("/bin/sh")
                    .arg("-c")
                    .arg(format!("echo $$ > {}", marker.display())),
                Reissue::Safe,
            );
        }

        let fast: Vec<i32> = (0..12)
            .map(|i| published_pid(&dir.path().join(format!("fast.{i}")), "a fast cleanup"))
            .collect();
        for pid in fast {
            wait_until_gone(pid, "a fast cleanup");
        }
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(blockers.pids[0]), None).is_ok(),
            "the blocked cleanups should still be blocked"
        );

        // And the reaper collects them too, once they end.
        blockers.kill_all();
        blockers.confirm_all_gone();

        // A one-shot cleanup is issued once however it ends, so nothing here
        // may have been started twice. Checked after the reap, once a retry
        // would have had its backoff and then some.
        std::thread::sleep(RETRY_BACKOFF * 2);
        for marker in &markers {
            assert_eq!(
                generations(marker).len(),
                1,
                "a one-shot cleanup was re-issued, which leaks a process the \
                 test never learns the pid of: {marker:?}"
            );
        }
    }

    /// A pass admits up to `REAP_BATCH` *arrivals*, whatever it is already
    /// holding.
    ///
    /// Driven through `admit_pass` -- the whole pass the reaper runs, opening
    /// receive and budget included -- against a plain channel, so there is no
    /// scheduler in it: the backlog is constructed rather than raced for. The
    /// pre-filled `held` is what the earlier budget counted, and against it
    /// that version admitted nothing at all -- so every queued arrival went
    /// back to costing a full scan, which is the quadratic the batch exists to
    /// prevent, in the backlog it exists for.
    #[test]
    fn admission_is_budgeted_by_arrivals_not_by_backlog() {
        let (tx, rx) = channel::<u32>();
        let queued = REAP_BATCH * 2;
        for i in 0..queued {
            tx.send(i as u32).expect("the receiver is alive");
        }

        // Already holding a full budget's worth of unfinished cleanups.
        let mut held: Vec<u32> = (0..REAP_BATCH as u32).collect();
        assert!(admit_pass(&rx, &mut held), "arrivals are still coming");

        assert_eq!(
            held.len(),
            REAP_BATCH * 2,
            "a full backlog must not stop the pass from admitting"
        );
        assert_eq!(
            rx.try_iter().count(),
            queued - REAP_BATCH,
            "the pass must stop at its budget rather than draining the queue"
        );
    }

    /// The receive that opens the pass spends budget too, so a pass never takes
    /// more than `REAP_BATCH` in total.
    #[test]
    fn the_opening_receive_counts_against_the_budget() {
        let (tx, rx) = channel::<u32>();
        let queued = REAP_BATCH + 1;
        for i in 0..queued {
            tx.send(i as u32).expect("the receiver is alive");
        }

        // Holding nothing, so the pass opens on the blocking receive -- which
        // returns at once here, the queue being full already.
        let mut held = Vec::new();
        assert!(admit_pass(&rx, &mut held), "arrivals are still coming");

        assert_eq!(held.len(), REAP_BATCH, "the opening receive is an arrival");
        assert_eq!(rx.try_iter().count(), queued - REAP_BATCH);
    }

    /// A closed channel ends the loop only when nothing is being held: the
    /// reaps of children already in hand are still owed, and finishing them
    /// needs no further arrival.
    #[test]
    fn a_disconnect_ends_the_pass_only_with_nothing_in_hand() {
        let (tx, rx) = channel::<u32>();
        drop(tx);

        let mut held = vec![1, 2, 3];
        assert!(
            admit_pass(&rx, &mut held),
            "children still in hand are still owed their reaps"
        );
        assert!(
            !admit_pass(&rx, &mut Vec::new()),
            "nothing can arrive and nothing is owed"
        );
    }

    /// A missing cleanup binary is not a panic. `Drop` is the caller, and a
    /// panic there during an unwind aborts the process.
    #[test]
    fn missing_cleanup_binary_does_not_panic() {
        detach_cleanup(Cmd::new("/nonexistent/outrig-cleanup-probe"), Reissue::Safe);
    }
}
