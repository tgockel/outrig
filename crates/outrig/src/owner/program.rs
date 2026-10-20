//! The program's process, while it runs: the signals forwarded to it, the grace it gets to end on
//! its own, and the end the engine gives a program that does not.
//!
//! The first `SIGINT` or `SIGTERM` the owner forwards reaches the program's process group, and
//! starts the grace. The grace is held while the engine shuts down a session the program asked it
//! to shut down, or cleans up after an entry it gave up on: the program is then doing what the
//! signal asked, and the time is the engine's. Past the grace, or at a second signal, the group is
//! stopped as a bindings process's is -- `SIGTERM`, [`STOP_GRACE`], `SIGKILL`, the group proven
//! empty -- and a further signal sends `SIGKILL` at once. Once the program has exited, the
//! connection's sessions are stopped by the connection's own task, which this tells through
//! `gone`.

use std::io::PipeWriter;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nix::sys::signal::Signal;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::python::supervisor::{STOP_GRACE, Supervised};

/// How often the grace is counted down.
const TICK: Duration = Duration::from_millis(100);

/// A signal an owner forwards to the program.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Forward {
    Interrupt,
    Terminate,
}

impl Forward {
    fn signal(self) -> Signal {
        match self {
            Forward::Interrupt => Signal::SIGINT,
            Forward::Terminate => Signal::SIGTERM,
        }
    }
}

/// What the program's task is given.
pub(crate) struct Supervision {
    pub(crate) child: Supervised,
    /// The engine's second write end, held for the program's life; see `python::program`.
    pub(crate) held: PipeWriter,
    /// The signals the owner forwards, until its handle is dropped.
    pub(crate) forwarded: mpsc::UnboundedReceiver<Forward>,
    pub(crate) grace: Duration,
    /// How many shutdowns the program asked for are running.
    pub(crate) asked: Arc<AtomicUsize>,
    /// The program's exit status, once it has one.
    pub(crate) exited: watch::Sender<Option<Result<ExitStatus, String>>>,
    /// Cancelled once the program has exited.
    pub(crate) gone: CancellationToken,
}

/// Supervise the program to its end, and say so.
pub(crate) async fn supervise(supervision: Supervision) {
    let Supervision {
        mut child,
        held,
        mut forwarded,
        grace,
        asked,
        exited,
        gone,
    } = supervision;
    let status = run(&mut child, &mut forwarded, grace, &asked).await;
    drop(held);
    exited.send_replace(Some(status.map_err(|e| e.to_string())));
    gone.cancel();
}

/// The program's life: waited for, and ended by the engine if it does not end after a forwarded
/// signal.
async fn run(
    child: &mut Supervised,
    forwarded: &mut mpsc::UnboundedReceiver<Forward>,
    grace: Duration,
    asked: &AtomicUsize,
) -> std::io::Result<ExitStatus> {
    // Running, until it exits or a first signal arrives.
    tokio::select! {
        status = child.wait() => return status,
        forward = forwarded.recv() => match forward {
            Some(forward) => child.signal(forward.signal()),
            // The owner let go of the program: it goes with the handle.
            None => return killed(child).await,
        },
    }

    // The grace, counted down only while no shutdown the program asked for runs.
    let mut left = grace;
    let mut ticks = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            status = child.wait() => return status,
            forward = forwarded.recv() => match forward {
                Some(_) => break,
                None => return killed(child).await,
            },
            _ = ticks.tick() => {
                if asked.load(Ordering::Relaxed) == 0 {
                    if left.is_zero() {
                        break;
                    }
                    left = left.saturating_sub(TICK);
                }
            }
        }
    }

    // Stopped as a bindings process is, unless a further signal asks for `SIGKILL` now.
    let at_once = {
        let stopping = child.stop(STOP_GRACE);
        tokio::pin!(stopping);
        tokio::select! {
            _ = &mut stopping => false,
            _ = forwarded.recv() => true,
        }
    };
    if at_once {
        child.kill().await;
    }
    child.wait().await
}

/// `SIGKILL` to the program's group, and its status once reaped.
async fn killed(child: &mut Supervised) -> std::io::Result<ExitStatus> {
    child.kill().await;
    child.wait().await
}
