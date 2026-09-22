//! Subprocess ownership, capture, and transcript support.
//!
//! The public piece is [`Transcript`], which mirrors command lines and output
//! into a log file for runtime startup paths. The generic command/capture
//! helpers in this module are crate-private implementation details used by
//! the container and image runtimes.
//!
//! # What owning a child guarantees
//!
//! Every process this crate spawns goes through [`Cmd::spawn_owned`] and comes
//! back as an [`Owned`]. Other modules are entitled to rely on two properties
//! of that.
//!
//! **Dropping the future terminates the child.** `Owned`'s destructor signals
//! synchronously -- the signal has been delivered by the time the drop
//! returns -- and hands the reap to a task on the runtime that spawned the
//! child, since a tokio `Child` is bound to its own runtime's signal driver.
//! The command also carries `kill_on_drop(true)`, which covers the case where
//! that task is never polled: the child is killed again (harmless) and
//! tokio's orphan queue reaps it instead.
//!
//! *Which* signal is the spawn's [`Termination`] claim. `Kill` is the default
//! and sends `SIGKILL`; `TermThenKill` sends `SIGTERM` and lets the reap task
//! escalate once a grace has passed without an exit. The bound is the same
//! either way -- something was delivered before the drop returned, and the
//! process will be gone -- but a graceful stop trades a bounded delay for the
//! chance that the child cleans up after itself on the way out. It is for the
//! one command that owns resources outrig cannot name; see [`Termination`].
//!
//! A grace is worth something only while this runtime is alive. A runtime
//! shutting down inside one drops the reap task, `kill_on_drop` escalates
//! immediately, and the child is back to being killed outright. Ctrl-C is not
//! covered by this mechanism at all and does not need to be: the terminal
//! delivers `SIGINT` to the whole foreground process group, so the child has
//! a catchable signal from the kernel before outrig does anything.
//!
//! So the bound a caller who never passes a stop signal gets is **terminated
//! synchronously, reaped as soon as the runtime is next driven** -- measured
//! in single-digit milliseconds in `process_tests`. It is deliberately not an
//! *instant* reap, and it is not a reap that can happen while the runtime is
//! blocked: `Drop` cannot await, so neither this nor tokio's own orphan queue
//! can promise more. A caller that drops a future and then blocks its runtime
//! thread will see the process dead but not yet reaped, which is why the
//! tests for this bound poll rather than read once.
//!
//! **A cooperating caller gets a confirmed reap.** The `*_until` helpers take
//! a stop signal. When it fires they kill the child and `wait` on it before
//! returning [`OutrigError::Canceled`], so the process is already gone -- and
//! already reaped -- when the caller sees the error.
//!
//! Nothing separates the two. [`Cmd::spawn_owned`] is a synchronous function,
//! so no `.await` sits between the `spawn` and the `Owned` that owns its
//! child, and there is therefore no instant at which the process exists and
//! nothing is responsible for it.
//!
//! # The one exception
//!
//! [`spawn_stdio`] hands its [`Child`] to the caller. The child keeps
//! `kill_on_drop(true)`, so dropping the handle still kills the process, but
//! the reap becomes the holder's -- this module does not supervise a child it
//! has given away. That is deliberate: `podman exec -i` callers want full
//! bidirectional control of the handle.
//!
//! # What this module does not cover
//!
//! Killing a client is not cleaning up what it created. A dead `podman exec`
//! leaves the process it started running inside the container, and a dead
//! `podman run` can leave a container behind, because the workload is
//! supervised by conmon in its own namespaces rather than by the client this
//! module owns. Engine-side obligations belong to [`crate::supervise`] and to
//! the scope guards in [`crate::container`] and [`crate::image`].

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use crate::error::{IoPathExt, OutrigError, Result};

pub(crate) const STDERR_TAIL_LIMIT: usize = 1024 * 1024;
const TRUNCATED_MARKER: &str = "... (truncated) ...\n";

/// What a child is sent when the future owning it is abandoned.
///
/// [`Termination::Kill`] is the default and is right for nearly everything:
/// a catchable signal buys nothing from a client that creates nothing outrig
/// cannot already name, and every engine-side resource outrig *can* name is
/// covered by a [`crate::supervise::CleanupGuard`] that removes it by a
/// selector only this attempt can be.
///
/// The exception is a command owning resources outrig cannot name at all.
/// `buildah build` creates a working container per stage; buildah offers no
/// way to label or name them, and `buildah containers --filter` selects only
/// on id, name, and ancestor -- so outrig can neither mark them going in nor
/// pick them out afterwards. Asking buildah to stop is what gets them removed
/// by the one process that holds their ids: buildah itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Termination {
    /// `SIGKILL`, delivered synchronously as the drop returns.
    Kill,
    /// `SIGTERM`, then `SIGKILL` once the grace has passed without an exit.
    ///
    /// The grace rides in the variant rather than being read from a constant
    /// so a test can pin a short one without sharing mutable state with the
    /// production value, and so the number stays with the caller that knows
    /// what it is waiting for rather than in here.
    TermThenKill(Duration),
}
const STREAM_READ_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Cmd {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<OsString>,
}

impl Cmd {
    pub(crate) fn new(program: &'static str) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub(crate) fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|s| s.as_ref().to_os_string()));
        self
    }

    /// Build a fresh `tokio::process::Command` from this argv. No stdio
    /// configuration is applied -- the caller layers `.stdin()` / `.stdout()`
    /// / `.stderr()` to taste before spawning.
    pub(crate) fn to_tokio_command(&self) -> Command {
        let mut c = Command::new(self.program);
        c.args(&self.args);
        c
    }

    /// Render the argv as a shell-like command line for diagnostics. This is
    /// display-only; callers must still spawn via `Command` so no quoting
    /// participates in execution.
    pub(crate) fn render(&self) -> String {
        std::iter::once(OsStr::new(self.program))
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(render_arg)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Spawn this command with `stdio`, returning the child already owned.
    ///
    /// The single chokepoint every spawn in this crate goes through, which is
    /// what makes ownership structural rather than something each call site
    /// has to remember. `kill_on_drop(true)` is applied here and nowhere else.
    ///
    /// `termination` is the one thing a call site does have to state. It is
    /// a claim about what the child owns that outrig does not -- not a
    /// preference -- so it is spelled at the spawn in the same way
    /// [`crate::supervise::Reissue`] is spelled at the cleanup, rather than
    /// guessed from an argv.
    ///
    /// Synchronous on purpose. An `async fn` would put a cancellation point
    /// between the `spawn` and the `Owned` that owns its child, which is the
    /// window this abstraction exists to close.
    pub(crate) fn spawn_owned(&self, stdio: StdioSpec, termination: Termination) -> Result<Owned> {
        // The reap has to run on the runtime that created the child, because
        // a tokio `Child` polls that runtime's SIGCHLD driver. Both this and
        // the spawn below need a runtime to be current; taking the handle
        // first means a caller who has none is told so here rather than from
        // inside tokio's process driver.
        let handle = Handle::current();

        let mut command = self.to_tokio_command();
        command
            .stdin(stdio.stdin)
            .stdout(stdio.stdout)
            .stderr(stdio.stderr)
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| self.spawn_error(e))?;
        // Duplicated here rather than where a caller takes the pipes, so the
        // invariant is "a graceful child keeps its write ends usable until
        // the reap" and not "... if somebody took its readers".
        let keepalive = match termination {
            Termination::Kill => Vec::new(),
            Termination::TermThenKill(_) => keep_pipes_open(&mut child),
        };
        Ok(Owned {
            child: Some(child),
            handle,
            termination,
            keepalive,
        })
    }

    /// Report that this command was stopped by its caller's signal. Consumes
    /// the argv, so it is the last thing a helper does on that path.
    fn canceled_error(self) -> OutrigError {
        OutrigError::Canceled {
            program: self.program,
            argv: self.args,
        }
    }

    /// Label a failure to start this command with the program and full argv.
    /// Without this a missing `podman` / `buildah` / `git` surfaces as a bare
    /// "No such file or directory (os error 2)" with nothing to act on.
    fn spawn_error(&self, source: std::io::Error) -> OutrigError {
        OutrigError::Spawn {
            program: self.program,
            command: self.render(),
            source,
        }
    }
}

/// The stdio a spawn wants. Built per call rather than stored, because
/// [`Stdio`] is a one-shot value: it can carry an owned file descriptor, so it
/// is neither `Clone` nor reusable across spawns.
pub(crate) struct StdioSpec {
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
}

impl StdioSpec {
    /// stdin closed, stdout and stderr piped -- what the capture helpers want.
    fn captured() -> Self {
        Self {
            stdin: Stdio::null(),
            stdout: Stdio::piped(),
            stderr: Stdio::piped(),
        }
    }

    /// [`Self::captured`] but with the parent's stdin. `try_capture` was built
    /// on `Command::output()`, which (unlike `std`'s) does not redirect stdin,
    /// so its children have always inherited it. Preserved deliberately:
    /// `Container::exec_capture` is public and runs through this.
    fn captured_inheriting_stdin() -> Self {
        Self {
            stdin: Stdio::inherit(),
            ..Self::captured()
        }
    }

    /// stdin closed, stdout inherited, stderr piped for line forwarding.
    fn streamed() -> Self {
        Self {
            stdout: Stdio::inherit(),
            ..Self::captured()
        }
    }

    /// All three piped, for callers driving the child both ways.
    pub(crate) fn bidirectional() -> Self {
        Self {
            stdin: Stdio::piped(),
            ..Self::captured()
        }
    }

    /// Send stderr somewhere other than a pipe -- a log file, in practice.
    pub(crate) fn with_stderr(self, stderr: Stdio) -> Self {
        Self { stderr, ..self }
    }
}

/// A child process this crate owns.
///
/// See the [module docs](self) for what that ownership guarantees. The short
/// version: dropping this kills the process synchronously and hands the reap
/// off, and [`Owned::terminate`] does both and waits.
#[derive(Debug)]
pub(crate) struct Owned {
    /// `None` once the child has been reaped by [`Owned::terminate`] or handed
    /// to a caller by [`Owned::into_child`]. Either way nothing is owed, and
    /// `Drop` has nothing to do.
    child: Option<Child>,
    handle: Handle,
    /// What the abandoning paths send. Fixed at the spawn, because the claim
    /// it encodes is about the command, not about the moment it is stopped.
    termination: Termination,
    /// Duplicates of the read ends of this child's pipes, held until it is
    /// reaped. Empty unless the policy is graceful.
    ///
    /// A stop is worth nothing to a child that cannot write. The helpers hand
    /// each pipe to a [`Drain`], which **aborts** when dropped, and a dropped
    /// future drops the drain before it drops this -- so the read end is gone
    /// before the signal goes out, and the child's next write to the pipe
    /// gets `EPIPE`. Go turns that into `SIGPIPE` on fds 1 and 2 and dies on
    /// the spot, which is precisely the abrupt ending the stop exists to
    /// avoid: measured against buildah 1.33.7, a build stopped this way exits
    /// on signal 13 and leaves the working container it was told to clean up.
    ///
    /// Holding a duplicate keeps the pipe open without reading it. After the
    /// drain is aborted nothing consumes it, so a child that wrote more than
    /// the pipe buffer would block -- bounded by the grace, and far past what
    /// a teardown writes.
    ///
    /// **This is why a drain may not stop before its stream ends.** Until the
    /// abort, the pipe's only protection against filling is that something is
    /// reading it; with this duplicate open, a drain that gives up early
    /// leaves the child blocked on a write with nobody coming, no
    /// cancellation, and no grace running. The helpers hold that up: neither
    /// invalid UTF-8 nor a failed transcript write ends a drain, and a read
    /// error only does because it means the pipe is already broken.
    keepalive: Vec<std::os::fd::OwnedFd>,
}

/// Duplicate the read end of each of `child`'s pipes, so they stay open for
/// as long as the duplicates are held. See [`Owned::keepalive`] for what
/// happens to a stopped child that cannot write.
///
/// A descriptor this cannot duplicate is dropped rather than raised: running
/// out is a condition the spawn above would already have failed on, and
/// degrading to the ungraceful ending is better than failing a build over it.
fn keep_pipes_open(child: &mut Child) -> Vec<std::os::fd::OwnedFd> {
    use std::os::fd::AsFd as _;

    let mut kept = Vec::new();
    let readers = [
        child.stdout.as_ref().map(|s| s.as_fd()),
        child.stderr.as_ref().map(|s| s.as_fd()),
    ];
    for reader in readers.into_iter().flatten() {
        match reader.try_clone_to_owned() {
            Ok(duplicate) => kept.push(duplicate),
            Err(e) => tracing::debug!(target: "outrig::process", error = %e, "pipe keepalive"),
        }
    }
    kept
}

/// Ask `child` to stop, reporting whether the signal was delivered.
///
/// Naming a pid is safe here and in few other places: this module has not
/// waited on the child, so the kernel is still holding its slot and cannot
/// have handed the number to anything else. The worst case is a process that
/// has already exited, whose zombie absorbs the signal harmlessly.
///
/// The signal goes to the pid and never to a process group. outrig shares its
/// group with everything it spawns, so `kill(-pgid, ..)` would be outrig
/// signalling itself and every sibling command in flight.
fn request_stop(child: &Child) -> bool {
    let Some(pid) = child.id() else {
        return false;
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .is_ok()
}

/// A stream-draining task that is **aborted** when dropped, rather than
/// detached.
///
/// Dropping a bare `JoinHandle` lets its task keep running, and a drain task
/// holds one end of the child's pipe. Killing the child does not necessarily
/// close the other end: anything the child left behind that inherited the
/// descriptor keeps it open, and the drain would go on reading -- and go on
/// growing its unbounded buffer -- long after the call that wanted the output
/// was abandoned. Aborting drops the read end instead, which is also what
/// tells such a descendant, via `EPIPE`, that nobody is listening.
struct Drain<T>(Option<tokio::task::JoinHandle<T>>);

impl<T: Send + 'static> Drain<T> {
    fn spawn(fut: impl Future<Output = T> + Send + 'static) -> Self {
        Self(Some(tokio::spawn(fut)))
    }

    /// Await the drain to completion.
    ///
    /// The handle is awaited **through** the guard and taken out only once it
    /// has resolved. Taking it first would move it into a temporary, and a
    /// caller cancelled during this await would drop that temporary -- which
    /// detaches the task instead of aborting it, leaving the reader, its
    /// descriptor and its buffer alive. That is exactly the case this join is
    /// most likely to be cancelled in: it is reached after the child exited,
    /// so if it is still running, something the child left behind is holding
    /// the pipe open.
    async fn join(mut self) -> T {
        let joined = {
            let handle = self
                .0
                .as_mut()
                .expect("the handle is present until it is joined");
            handle.await
        };
        // Resolved, so there is nothing left for `Drop` to abort.
        self.0 = None;
        joined.expect("stream capture task panicked")
    }
}

impl<T> Drop for Drain<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

/// How a child stopped, when it was awaited under a stop signal.
enum Waited {
    Exited(ExitStatus),
    /// The stop signal fired first. The child was killed **and reaped** before
    /// this was produced, so a caller holding it has a finished process, not a
    /// kill in flight.
    Canceled,
}

impl Owned {
    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("the child is present until it is reaped or released")
    }

    pub(crate) fn take_stdin(&mut self) -> ChildStdin {
        self.child_mut()
            .stdin
            .take()
            .expect("stdin was configured as piped")
    }

    pub(crate) fn take_stdout(&mut self) -> ChildStdout {
        self.child_mut()
            .stdout
            .take()
            .expect("stdout was configured as piped")
    }

    pub(crate) fn take_stderr(&mut self) -> ChildStderr {
        self.child_mut()
            .stderr
            .take()
            .expect("stderr was configured as piped")
    }

    /// Await the child's exit. The ordinary success path.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child_mut().wait().await
    }

    /// Await the child, unless `stop` completes first -- in which case kill it
    /// and await the reap before returning [`Waited::Canceled`].
    ///
    /// `stop` is any future: `CancellationToken::cancelled()` for a caller
    /// with a token, `tokio::time::sleep(..)` for one with a budget, and
    /// `std::future::pending()` for one with neither.
    async fn wait_until(&mut self, stop: impl Future<Output = ()>) -> std::io::Result<Waited> {
        let exited = {
            let child = self.child_mut();
            tokio::select! {
                status = child.wait() => Some(status?),
                () = stop => None,
            }
        };
        match exited {
            Some(status) => Ok(Waited::Exited(status)),
            None => {
                self.terminate().await?;
                Ok(Waited::Canceled)
            }
        }
    }

    /// Kill the child and wait for it. Returns only once the process is gone
    /// and reaped, which is the guarantee `Drop` cannot make.
    ///
    /// Idempotent, and safe to follow with [`Owned::wait`]: tokio caches the
    /// exit status on the `Child`, so a second wait returns it rather than
    /// asking the kernel about a pid nobody owns any more. `Drop` sees the
    /// same cached status through `try_wait` and does nothing.
    pub(crate) async fn terminate(&mut self) -> std::io::Result<()> {
        // Under a graceful policy the caller is present to wait, so the stop
        // is awaited here rather than handed to a detached task -- the same
        // sequence `Drop` runs, with the confirmation `Drop` cannot give.
        if let Termination::TermThenKill(grace) = self.termination
            && request_stop(self.child_mut())
            && let Ok(waited) = tokio::time::timeout(grace, self.child_mut().wait()).await
        {
            waited?;
            return Ok(());
        }
        // The only `start_kill` failure is "already exited", which is the
        // state this is trying to reach.
        let _ = self.child_mut().start_kill();
        self.child_mut().wait().await?;
        Ok(())
    }

    /// Release the child to the caller, who takes on the reap. Used only by
    /// [`spawn_stdio`]; see the module docs for why it is the exception.
    pub(crate) fn into_child(mut self) -> Child {
        self.child
            .take()
            .expect("the child is present until it is reaped or released")
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        // A child that already exited is already reaped by this call, so
        // nothing is owed and there is no task worth spawning.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Synchronous, so termination does not wait on a task being
        // scheduled. `kill_on_drop` would send this too, but only once
        // `child` is dropped -- which is after the reap task below has been
        // handed it, or after that task has itself been dropped.
        //
        // Which signal goes out depends on the policy, but *a* signal goes
        // out before this returns either way: the bound other modules rely on
        // is "terminated synchronously", and a graceful policy changes what
        // was sent, not whether anything was. A `SIGTERM` that could not be
        // delivered falls through to the kill rather than leaving the child
        // unsignalled.
        let grace = if let Termination::TermThenKill(grace) = self.termination
            && request_stop(&child)
        {
            Some(grace)
        } else {
            // The only `start_kill` failure is "already exited", which is the
            // state this is trying to reach.
            let _ = child.start_kill();
            None
        };
        // Spawning on a runtime that has already shut down does not panic;
        // the task is dropped instead. That drops `child`, which tokio puts
        // on its orphan queue to reap on a later SIGCHLD -- so the reap still
        // happens, by the backstop rather than by this task. `kill_on_drop`
        // is what makes sure the kill has been sent by then; the queueing
        // happens either way.
        //
        // That backstop is also the limit of a grace: it is worth something
        // only while this runtime is alive. A runtime shutting down inside
        // one drops the task, and `kill_on_drop` escalates immediately --
        // correct, and the reason the grace is not a promise.
        //
        // The keepalives ride along and are closed by this task's end, which
        // is after the reap: a child asked to stop has to be able to write
        // until it is gone.
        let keepalive = std::mem::take(&mut self.keepalive);
        self.handle.spawn(async move {
            if let Some(grace) = grace
                && tokio::time::timeout(grace, child.wait()).await.is_err()
            {
                let _ = child.start_kill();
            }
            let _ = child.wait().await;
            drop(keepalive);
        });
    }
}

#[derive(Clone, Debug)]
pub struct Transcript {
    file: Arc<Mutex<tokio::fs::File>>,
    stderr: bool,
}

impl Transcript {
    /// Create a new transcript file, truncating any stale content at `path`.
    /// When `stderr` is true, every transcript line is also mirrored to the
    /// process's stderr.
    pub async fn create(path: &Path, stderr: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .path_ctx("create directory", parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await
            .path_ctx("create", path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            stderr,
        })
    }

    /// Record one already-rendered logical line with the conventional
    /// `[prefix]` marker.
    pub async fn line(&self, prefix: &'static str, line: &str) -> std::io::Result<()> {
        let rendered = format!("[{prefix}] {line}\n");
        self.write_all(rendered.as_bytes()).await
    }

    async fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        if self.stderr {
            let mut stderr = tokio::io::stderr();
            stderr.write_all(bytes).await?;
            stderr.flush().await?;
        }
        let mut file = self.file.lock().await;
        file.write_all(bytes).await?;
        file.flush().await
    }
}

/// Spawn the command, capture stdout and stderr, and return the `Output`
/// regardless of exit status. Only true I/O / spawn failures propagate as
/// errors. Use this when a non-zero exit is meaningful information to the
/// caller (e.g. `git rev-parse --git-dir` for "is this a git repo?",
/// `buildah images --quiet TAG` for "does this tag exist?") rather than an
/// error condition.
pub(crate) async fn try_capture(cmd: Cmd) -> Result<Output> {
    let mut child = cmd.spawn_owned(StdioSpec::captured_inheriting_stdin(), Termination::Kill)?;
    let stdout_task = Drain::spawn(capture_all(child.take_stdout()));
    let stderr_task = Drain::spawn(capture_all(child.take_stderr()));

    // This used `Command::output()`, which folds a mid-read I/O failure into
    // the same error as a failure to start. Preserved rather than refined:
    // `Container::exec_capture` is public and reports through here.
    let status = child.wait().await.map_err(|e| cmd.spawn_error(e))?;

    let stdout = stdout_task.join().await.map_err(|e| cmd.spawn_error(e))?;
    let stderr = stderr_task.join().await.map_err(|e| cmd.spawn_error(e))?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn the command, capture stdout and stderr, and optionally tee a
/// transcript of the command line plus both output streams. Non-zero exit is
/// returned in the `Output`, matching [`try_capture`].
pub(crate) async fn try_capture_logged(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
) -> Result<Output> {
    try_capture_logged_until(
        cmd,
        prefix,
        transcript,
        Termination::Kill,
        std::future::pending(),
    )
    .await
}

/// [`try_capture_logged`], stoppable.
///
/// The only stoppable helper, because it is the only one with a caller that
/// passes a real signal ([`crate::container::Container::stop`]). The others
/// would be the same four lines; adding them is what the day a caller needs
/// one is for. The transcript keeps whatever the child wrote before the
/// signal, and the tee tasks are dropped rather than drained -- a child that
/// leaked its pipes to a grandchild would otherwise hold the drain open past
/// the reap this promises.
///
/// The signal covers the **whole** call, exit included, and not just the wait
/// on the child. A pipe outlives the process that was given it: a child can
/// exit on time while a descendant it leaked stdout to holds the read end
/// open, and the drain that follows then blocks on a stream nobody will close.
/// Retiring the signal at the child's exit would leave that final stretch
/// unbounded -- which is precisely where `Drain`'s own docs place the most
/// likely cancellation -- so a caller with a budget could hang past it having
/// already got what it asked for. A stop landing there reports `Canceled` like
/// any other: the command ran, but its output cannot be produced, and a caller
/// that re-dispatches on `Canceled` loses nothing by doing so again.
pub(crate) async fn try_capture_logged_until(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
    termination: Termination,
    stop: impl Future<Output = ()>,
) -> Result<Output> {
    let transcript = transcript.cloned();
    if let Some(t) = &transcript {
        t.line(prefix, &format!("$ {}", cmd.render())).await?;
    }

    // The `-v` transcript is opt-in and file-backed; this pair is what makes a
    // stuck `podman run` visible under `RUST_LOG=debug` alone.
    tracing::debug!(target: "outrig::process", command = %cmd.render(), "spawn");
    let started = Instant::now();

    let mut child = cmd.spawn_owned(StdioSpec::captured(), termination)?;
    let stdout_task = Drain::spawn(capture_stream(
        child.take_stdout(),
        prefix,
        transcript.clone(),
    ));
    let stderr_task = Drain::spawn(capture_stream(child.take_stderr(), prefix, transcript));

    // Pinned so the same signal can be awaited twice: once against the child,
    // and again against the drain that outlives it.
    tokio::pin!(stop);

    let Waited::Exited(status) = child.wait_until(&mut stop).await? else {
        // The drains abort as they drop, which is what closes the read ends.
        return Err(cmd.canceled_error());
    };
    tracing::debug!(
        target: "outrig::process",
        program = cmd.program,
        code = ?status.code(),
        elapsed_ms = started.elapsed().as_millis(),
        "exit"
    );

    // Both drains are joined *inside* one branch, so a stop dropping it drops
    // them together and each aborts through its guard. Biased, so a drain that
    // finished in the same instant the budget expired is reported as the
    // output it is rather than as a cancellation.
    let (stdout, stderr) = tokio::select! {
        biased;
        drained = async {
            let stdout = stdout_task.join().await?;
            let stderr = stderr_task.join().await?;
            Ok::<_, std::io::Error>((stdout, stderr))
        } => drained?,
        () = &mut stop => return Err(cmd.canceled_error()),
    };

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn the command, capture stdout and stderr, and return the `Output` on
/// success. On non-zero (or signal) exit, return [`OutrigError::Process`] with
/// the program, argv, exit code, and the last `STDERR_TAIL_LIMIT` bytes of
/// stderr (lossy UTF-8, prefixed with a truncation marker if elision occurred).
pub(crate) async fn run_capture(cmd: Cmd) -> Result<Output> {
    tracing::debug!(target: "outrig::process", command = %cmd.render(), "spawn");
    let started = Instant::now();

    let mut child = cmd.spawn_owned(StdioSpec::captured(), Termination::Kill)?;
    let stdout_task = Drain::spawn(capture_all(child.take_stdout()));
    let stderr_task = Drain::spawn(capture_stderr_tail(child.take_stderr()));

    let status = child.wait().await?;
    tracing::debug!(
        target: "outrig::process",
        program = cmd.program,
        code = ?status.code(),
        elapsed_ms = started.elapsed().as_millis(),
        "exit"
    );
    let stdout = stdout_task.join().await?;
    let stderr_tail = stderr_task.join().await?;

    if status.success() {
        Ok(Output {
            status,
            stdout,
            stderr: stderr_tail.into_bytes(),
        })
    } else {
        Err(OutrigError::Process {
            program: cmd.program,
            argv: cmd.args,
            exit_code: status.code(),
            stderr_tail: stderr_tail.into_tail_string(),
        })
    }
}

/// Logged sibling of [`run_capture`]. On success, returns captured output;
/// on non-zero, returns the same structured process error with a stderr tail.
pub(crate) async fn run_capture_logged(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
) -> Result<Output> {
    run_capture_logged_terminating(cmd, prefix, transcript, Termination::Kill).await
}

/// [`run_capture_logged`] for a caller that owns the termination claim.
///
/// Separate rather than a fourth parameter on the common helper: `Kill` is
/// right for every one of its callers but the build, and a parameter on all
/// of them would bury the single exception in a column of identical
/// arguments instead of naming it.
pub(crate) async fn run_capture_logged_terminating(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
    termination: Termination,
) -> Result<Output> {
    let output = try_capture_logged_until(
        cmd.clone(),
        prefix,
        transcript,
        termination,
        std::future::pending(),
    )
    .await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(OutrigError::Process {
            program: cmd.program,
            argv: cmd.args,
            exit_code: output.status.code(),
            stderr_tail: tail_string(&output.stderr, STDERR_TAIL_LIMIT),
        })
    }
}

/// Spawn the command with stderr piped, forwarding each stderr line to
/// `tracing::info!` as `[<prefix>] <line>` (target `outrig::process`). stdout
/// inherits the parent's; stdin is null. Returns the [`ExitStatus`] -- a
/// non-zero exit is **not** an error, since callers may want to inspect
/// status before deciding what it means.
pub(crate) async fn run_streamed(
    cmd: Cmd,
    prefix: &'static str,
    termination: Termination,
) -> Result<ExitStatus> {
    let mut child = cmd.spawn_owned(StdioSpec::streamed(), termination)?;
    let stderr = child.take_stderr();

    let log_task = Drain::spawn(async move {
        // Bytes then lossy, rather than `lines()`, because a drain that
        // stops early while the child is still writing is a hang. Nothing
        // reads the pipe after that, and for a child whose read end this
        // module holds open (see [`Owned::keepalive`]) an unread pipe fills
        // at 64 KiB and blocks the writer -- with `wait` below still ahead
        // of the join, on the ordinary path, with no cancellation and no
        // grace to end it. `lines()` returns `Err` for invalid UTF-8, which
        // a `RUN` writing binary to stderr produces, so that was reachable
        // by a build simply being noisy.
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            // A read error means the pipe itself is broken, so there is
            // nothing left to drain and nothing that can block on it.
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let rendered = String::from_utf8_lossy(&line);
            tracing::info!(
                target: "outrig::process",
                "[{prefix}] {}",
                rendered.trim_end_matches(['\r', '\n'])
            );
        }
    });

    let status = child.wait().await?;
    log_task.join().await;
    Ok(status)
}

/// [`run_streamed`] for a caller that treats a non-zero exit as an error.
///
/// The streamed counterpart of [`run_capture`]'s exit check. There is no
/// stderr tail to offer -- it was forwarded to `tracing` line by line as the
/// command ran, which is the point of streaming -- so the error carries the
/// argv and the code, and the output is already in the log above it.
pub(crate) async fn run_streamed_checked(
    cmd: Cmd,
    prefix: &'static str,
    termination: Termination,
) -> Result<()> {
    let program = cmd.program;
    let argv = cmd.args.clone();
    let status = run_streamed(cmd, prefix, termination).await?;
    if status.success() {
        Ok(())
    } else {
        Err(OutrigError::Process {
            program,
            argv,
            exit_code: status.code(),
            stderr_tail: String::new(),
        })
    }
}

/// Spawn the command with all three of stdin/stdout/stderr piped, returning
/// the [`Child`]. Used by `podman exec -i` callers that want full
/// bidirectional control.
///
/// **This is the module's one ownership exception.** The child is spawned
/// `kill_on_drop(true)`, so dropping the returned handle SIGKILLs the process
/// -- but the reap is the caller's, because this module does not supervise a
/// child it has handed away. Nor does killing the client stop what it started:
/// a dead `podman exec` leaves its in-container process running under conmon.
pub(crate) async fn spawn_stdio(cmd: Cmd) -> Result<Child> {
    Ok(cmd
        .spawn_owned(StdioSpec::bidirectional(), Termination::Kill)?
        .into_child())
}

pub(crate) fn tail_string(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let start = bytes.len() - limit;
        let mut out = String::with_capacity(limit + TRUNCATED_MARKER.len());
        out.push_str(TRUNCATED_MARKER);
        out.push_str(&String::from_utf8_lossy(&bytes[start..]));
        out
    }
}

async fn capture_all<R>(stream: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut captured = Vec::new();
    reader.read_to_end(&mut captured).await?;
    Ok(captured)
}

async fn capture_stderr_tail<R>(stream: R) -> std::io::Result<BoundedStderrTail>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut captured = BoundedStderrTail::new();
    let mut chunk = [0_u8; STREAM_READ_CHUNK];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        captured.push(&chunk[..n]);
    }
    Ok(captured)
}

#[derive(Debug)]
struct BoundedStderrTail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl BoundedStderrTail {
    fn new() -> Self {
        Self {
            bytes: VecDeque::with_capacity(STDERR_TAIL_LIMIT),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() > STDERR_TAIL_LIMIT {
            self.bytes.clear();
            self.bytes
                .extend(chunk[chunk.len() - STDERR_TAIL_LIMIT..].iter().copied());
            self.truncated = true;
            return;
        }

        let overflow = self.bytes.len() + chunk.len();
        if overflow > STDERR_TAIL_LIMIT {
            self.bytes.drain(..overflow - STDERR_TAIL_LIMIT);
            self.truncated = true;
        }
        self.bytes.extend(chunk.iter().copied());
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }

    fn into_tail_string(self) -> String {
        let truncated = self.truncated;
        let bytes = self.into_bytes();
        if truncated {
            let mut out = String::with_capacity(bytes.len() + TRUNCATED_MARKER.len());
            out.push_str(TRUNCATED_MARKER);
            out.push_str(&String::from_utf8_lossy(&bytes));
            out
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

async fn capture_stream<R>(
    stream: R,
    prefix: &'static str,
    transcript: Option<Transcript>,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut captured = Vec::new();
    // Reported once the stream is drained, never in place of draining it. A
    // transcript on a full disk used to end this loop with the child still
    // writing, which for a child whose read end this module holds open is a
    // stall rather than an error -- see the drain in [`run_streamed`].
    let mut transcript_failed = None;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            break;
        }
        captured.extend_from_slice(&line);
        if let Some(t) = &transcript
            && transcript_failed.is_none()
        {
            let rendered = String::from_utf8_lossy(&line);
            let rendered = rendered.trim_end_matches(['\r', '\n']);
            if let Err(e) = t.line(prefix, rendered).await {
                transcript_failed = Some(e);
            }
        }
    }
    match transcript_failed {
        Some(e) => Err(e),
        None => Ok(captured),
    }
}

fn render_arg(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(is_shell_safe_byte) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn is_shell_safe_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'/' | b'.' | b'-' | b'_' | b':' | b'=' | b',' | b'+' | b'@' | b'%'
        )
}

#[cfg(test)]
#[path = "process_tests.rs"]
pub(crate) mod process_tests;
