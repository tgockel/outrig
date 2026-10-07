//! The signals that end a session -- SIGINT, SIGTERM, SIGHUP -- and how each
//! phase of `outrig run` and `outrig mcp` hears them.
//!
//! A signal left at its default disposition kills outrig where it stands, and
//! nothing that cleans up after a session runs: the containers live on under
//! conmon, and the session record is never finalized, so `outrig discard`
//! refuses it as still running and `outrig clean` skips it. Session setup
//! installs [`Signals`] just before it writes the record -- the first thing a
//! signal could orphan -- and every phase from there that waits on something
//! slow is raced against it. `outrig image build` does the same from the
//! moment it starts its validation container.
//!
//! Two properties of tokio's listeners carry the design:
//!
//! - A signal that lands while a listener is not being polled is returned by
//!   that listener's next `recv`. A phase that is not raced therefore defers a
//!   signal to the next phase that is; it never loses it, and once a listener
//!   exists the signal no longer kills the process.
//! - A listener sees only the signals delivered after it was created. Cleanup
//!   races a *fresh* set ([`unless_signaled`]), so the signal that ended the
//!   session -- or a Ctrl-C typed at the REPL -- is not mistaken for a second
//!   one asking cleanup to hurry.
//!
//! Notices go through `writeln!` with the error dropped, never `eprintln!`:
//! after SIGHUP the terminal may be gone, and `eprintln!` panics when the
//! write fails.

use std::fmt;
use std::future::Future;
use std::io::Write as _;

use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::error::{CliError, OutrigError, Result};

/// A signal that ends a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndSignal {
    Hangup,
    Interrupt,
    Terminate,
}

impl EndSignal {
    fn kind(self) -> SignalKind {
        match self {
            EndSignal::Hangup => SignalKind::hangup(),
            EndSignal::Interrupt => SignalKind::interrupt(),
            EndSignal::Terminate => SignalKind::terminate(),
        }
    }

    /// The shell's code for a process this signal killed, 128 plus the signal
    /// number: 129, 130, 143. A session a signal ended exits with it, and its
    /// record keeps it, so the two read the same as they would had the signal
    /// killed outrig outright.
    pub fn exit_code(self) -> i32 {
        128 + self.kind().as_raw_value()
    }
}

impl fmt::Display for EndSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EndSignal::Hangup => "SIGHUP",
            EndSignal::Interrupt => "SIGINT",
            EndSignal::Terminate => "SIGTERM",
        })
    }
}

/// Listeners for the three signals that end a session, held for the life of
/// the session so that none of them is ever at its default disposition.
pub struct Signals {
    hangup: Signal,
    interrupt: Signal,
    terminate: Signal,
}

impl Signals {
    /// Replace the default disposition of SIGINT, SIGTERM and SIGHUP for the
    /// rest of the process. Call before anything a signal would orphan exists.
    pub fn install() -> Result<Self> {
        let listen = |sig: EndSignal| signal(sig.kind()).map_err(OutrigError::Io);
        Ok(Self {
            hangup: listen(EndSignal::Hangup)?,
            interrupt: listen(EndSignal::Interrupt)?,
            terminate: listen(EndSignal::Terminate)?,
        })
    }

    /// The next SIGINT, SIGTERM, or SIGHUP.
    pub async fn any(&mut self) -> EndSignal {
        self.next(true).await
    }

    /// The next SIGTERM or SIGHUP. For the REPL, which owns SIGINT: there a
    /// Ctrl-C cancels the turn in flight, and only a second one ends the
    /// session.
    pub async fn termination(&mut self) -> EndSignal {
        self.next(false).await
    }

    async fn next(&mut self, interrupt: bool) -> EndSignal {
        // `recv` returns `None` only once the runtime's signal driver is gone,
        // and then no signal can arrive at all.
        tokio::select! {
            Some(()) = self.interrupt.recv(), if interrupt => EndSignal::Interrupt,
            Some(()) = self.terminate.recv() => EndSignal::Terminate,
            Some(()) = self.hangup.recv() => EndSignal::Hangup,
            else => std::future::pending().await,
        }
    }

    /// Run `fut` unless a signal arrives first. The signal wins a tie, and
    /// ends the wait as [`CliError::Interrupted`] with `fut` dropped.
    ///
    /// Dropping is what the library's layers are built for: a container start
    /// in flight is removed by its start guard, a started container by its
    /// `Drop`, an interceptor attachment by its rollback, and a child process
    /// by its owner. Whatever the caller holds outside `fut` is the caller's to
    /// stop, through the same bail-out an ordinary failure takes.
    pub async fn race<T, E>(
        &mut self,
        fut: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T>
    where
        CliError: From<E>,
    {
        self.race_ending("the session", fut).await
    }

    /// [`Self::race`] for a command that is not a session, whose notice names
    /// `ending` -- what the signal cuts short -- in place of the session.
    pub async fn race_ending<T, E>(
        &mut self,
        ending: &str,
        fut: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T>
    where
        CliError: From<E>,
    {
        tokio::select! {
            biased;
            sig = self.any() => Err(interrupted(sig, ending)),
            result = fut => result.map_err(CliError::from),
        }
    }
}

/// The error a signal ends a wait with, announced as it happens: the teardown
/// that follows can take seconds, and the user should not have to wonder what
/// it is for.
fn interrupted(sig: EndSignal, ending: &str) -> CliError {
    notice(&format!("[outrig] {sig} received; ending {ending}"));
    CliError::Interrupted(sig)
}

/// [`interrupted`], for a signal that lands where the line is unfinished: at
/// the REPL's prompt, or under a reply still streaming in. The notice starts
/// on a line of its own, as the REPL's `[outrig] interrupted` does.
pub fn interrupted_mid_line(sig: EndSignal) -> CliError {
    notice("");
    interrupted(sig, "the session")
}

/// Run a session's container cleanup to completion, unless SIGINT or SIGTERM
/// arrives first. Then `cleanup` is dropped, and returns that signal.
///
/// Dropping hands what `cleanup` had not stopped yet to the `Drop` layers: a
/// detached `podman rm -f` per container, which outlives outrig itself, and
/// the interceptor's detached rollback. That is faster than an orderly stop
/// and still leaves nothing behind -- which is what someone sending a second
/// signal is asking for.
///
/// SIGHUP does not cut cleanup short. It means the terminal is gone, so there
/// is nobody waiting to be impatient, and a closed terminal can deliver it
/// twice in quick succession: once from the kernel and once from the shell
/// passing it on.
pub async fn unless_signaled(cleanup: impl Future<Output = ()>) -> Option<EndSignal> {
    let mut fresh = match Signals::install() {
        Ok(signals) => signals,
        Err(e) => {
            tracing::warn!(
                target: "outrig::cli::signals",
                "cannot listen for signals during cleanup: {e}"
            );
            cleanup.await;
            return None;
        }
    };
    let impatient = async {
        tokio::select! {
            Some(()) = fresh.interrupt.recv() => EndSignal::Interrupt,
            Some(()) = fresh.terminate.recv() => EndSignal::Terminate,
            else => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        () = cleanup => None,
        sig = impatient => {
            notice(&format!(
                "[outrig] {sig} received during teardown; removing the session's containers \
                 in the background"
            ));
            Some(sig)
        }
    }
}

fn notice(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}
