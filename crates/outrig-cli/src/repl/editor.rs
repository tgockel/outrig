//! rustyline-backed [`LineSource`]: the interactive prompt.
//!
//! Gives the REPL readline-style editing and recall of the prompts typed
//! earlier in this session. History is in memory and never reaches the disk --
//! that is enforced by the Cargo feature set rather than by discipline, since
//! `default-features = false` drops `with-file-history` and so makes
//! rustyline's `DefaultHistory` a `MemHistory`.
//!
//! Two properties of rustyline shape everything here:
//!
//! - `Behavior::PreferTerm` makes it open `/dev/tty` for both reading and
//!   writing, so the prompt and the echo of what is typed touch neither stdout
//!   nor stderr. That is what keeps `outrig run > out.txt` capturing only the
//!   model's replies, which a default-configured editor -- writing to stdout --
//!   would break.
//! - `readline` blocks, and it is the one call that can be outstanding for an
//!   hour while the user reads code. It runs on a dedicated thread that owns
//!   the editor for the REPL's lifetime, not on tokio's blocking pool: a pool
//!   worker parked in `read()` is one `BlockingPool::drop` joins without a
//!   timeout, which would wedge process exit.

use std::fs::File;
use std::future::Future;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use rustyline::error::{ReadlineError, Signal};
use rustyline::{Behavior, Config, DefaultEditor};
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;

use super::line_source::{LineEvent, LineSource};
use super::notice;
use crate::error::Result;

/// Terminals rustyline refuses to drive. On any of these `readline` takes its
/// `readline_direct` path, which writes the prompt to *stdout* -- so the check
/// has to happen out here, before an editor exists, or `TERM=dumb outrig run >
/// out.txt` would put `> ` in the captured output.
///
/// This mirrors `UNSUPPORTED_TERM` in rustyline's `src/tty/mod.rs`, which is
/// private, as is the `Term::is_unsupported` that reads it. There is no public
/// way to ask -- `Editor::dimensions()` keys off `is_output_tty`, which under
/// `PreferTerm` answers for the `/dev/tty` rustyline opened and so says "fine"
/// on `TERM=dumb`. Predicting a private list is the only option available, so
/// **re-check it on every rustyline upgrade**: if upstream adds a name, outrig
/// hands that terminal an editor whose prompt lands in redirected stdout, and
/// no test here can see it.
const UNSUPPORTED_TERMS: [&str; 3] = ["dumb", "cons25", "emacs"];

/// Prompts recalled by Up/Down. In memory, for one session.
const HISTORY_SIZE: usize = 500;

/// Whether rustyline can drive this terminal.
///
/// `stdin_is_tty` is outrig's own call to make and cannot be delegated: under
/// `PreferTerm` rustyline opened `/dev/tty` itself, so its internal checks
/// answer for that and say yes even when outrig's stdin is a pipe.
fn tty_capable(stdin_is_tty: bool, term: Option<&str>) -> bool {
    stdin_is_tty
        && !term.is_some_and(|term| {
            UNSUPPORTED_TERMS
                .iter()
                .any(|unsupported| unsupported.eq_ignore_ascii_case(term))
        })
}

/// Anti-hang backstop on [`TerminalLease::await_quiescence`], not a budget the
/// reader is expected to need. Both states it waits for are reachable in
/// microseconds; reaching this instead means an assumption below is wrong, and
/// it says so rather than restoring on a guess.
const QUIESCENCE_BACKSTOP: Duration = Duration::from_secs(5);

/// What the reader was doing when the guard asked.
enum Quiescence {
    /// Not reading, and `stopped` keeps it that way.
    Idle,
    /// Inside `readline`, which means raw mode is on and cannot be entered a
    /// second time before the process goes away.
    Reading,
    /// Neither, for longer than should be possible.
    Unresolved,
}

/// Serializes "a read is running" against "the terminal is being restored".
///
/// The reader thread holds `reading` across `readline`, which is exactly the
/// span in which rustyline has raw mode on, and re-checks `stopped` *under*
/// that lock rather than before taking it.
struct TerminalLease {
    reading: Mutex<()>,
    stopped: AtomicBool,
}

impl TerminalLease {
    fn new() -> Self {
        Self {
            reading: Mutex::new(()),
            stopped: AtomicBool::new(false),
        }
    }

    /// Take the terminal for one read, or `None` when the session is stopping.
    ///
    /// The `stopped` check happens *under* the lock, not before it: a
    /// cancellation landing between the two would otherwise let a read start
    /// and turn raw mode back on after the terminal had been restored. The
    /// returned guard must be held for as long as the read runs.
    fn acquire(&self) -> Option<std::sync::MutexGuard<'_, ()>> {
        let reading = self
            .reading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!self.stopped.load(Ordering::SeqCst)).then_some(reading)
    }

    /// Wait until restoring the terminal will actually stick.
    ///
    /// Taking the lease is not enough on its own. A reader that has taken it
    /// but has not yet reached rustyline's `enable_raw_mode` is *committed* to
    /// turning raw mode on, and restoring in that window would simply be undone
    /// -- so the wait is for one of two states that can be observed rather than
    /// assumed, and both of which are reachable in microseconds:
    ///
    /// - the lease is free, so `stopped` has already stopped the reader, or
    /// - raw mode is on, so the reader is inside `readline` and cannot enter it
    ///   again.
    ///
    /// Those are exhaustive after `acquire` succeeds, because `readline` either
    /// enables raw mode or fails and releases the lease. `is_raw` is injected
    /// so the ordering can be tested without a terminal.
    fn await_quiescence(&self, is_raw: &dyn Fn() -> bool, backstop: Duration) -> Quiescence {
        let deadline = Instant::now() + backstop;
        loop {
            if self.reading.try_lock().is_ok() {
                return Quiescence::Idle;
            }
            if is_raw() {
                return Quiescence::Reading;
            }
            if Instant::now() >= deadline {
                return Quiescence::Unresolved;
            }
            std::thread::yield_now();
        }
    }
}

/// The controlling terminal's mode as it was before rustyline touched it.
///
/// rustyline restores the mode when `readline` *returns*, from a guard local to
/// that call. Nothing restores it when the REPL future is dropped mid-read and
/// the process hard-exits -- which is exactly what happens when the session
/// watcher sees the primary container die
/// ([`watcher::exit_if_monitor_stopped`](crate::cli::watcher)). Without this
/// the user would land back in a shell with `ECHO` and `ICANON` still cleared,
/// and would have to type `stty sane` blind.
///
/// It works because it lives in [`EditorSource`], on the REPL future's stack:
/// dropping that future runs it, ahead of teardown and of any `process::exit`.
struct TerminalModeGuard {
    tty: File,
    saved: Termios,
    lease: Arc<TerminalLease>,
}

impl TerminalModeGuard {
    fn capture(lease: Arc<TerminalLease>) -> std::io::Result<Self> {
        let tty = File::options().read(true).write(true).open("/dev/tty")?;
        let saved = termios::tcgetattr(&tty)?;
        Ok(Self { tty, saved, lease })
    }

    /// Whether the terminal is in raw mode *now*, which for this process means
    /// a `readline` is running. Read back from the terminal rather than tracked
    /// in a flag of our own: rustyline is what sets it, and the question being
    /// asked is precisely "has it done so yet".
    fn is_raw(&self) -> bool {
        termios::tcgetattr(&self.tty)
            .map(|mode| !mode.local_flags.contains(LocalFlags::ICANON))
            .unwrap_or(false)
    }
}

impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        // Ordered before everything else: the reader re-reads this under
        // `reading`, so a read that has not started by now never will.
        self.lease.stopped.store(true, Ordering::SeqCst);

        // Restoring blind would race the reader. Dropping the request channel
        // does not help -- `mpsc::recv` still hands over an already-queued
        // request after the sender is gone -- so a cancellation landing between
        // `send` and the start of `readline` would re-enable raw mode *after*
        // this restore, and the `process::exit` on that path skips rustyline's
        // own cleanup.
        if let Quiescence::Unresolved = self
            .lease
            .await_quiescence(&|| self.is_raw(), QUIESCENCE_BACKSTOP)
        {
            tracing::warn!(
                target: "outrig::repl",
                "line editor did not settle before teardown; the terminal may need `stty sane`",
            );
        }

        // TCSANOW rather than rustyline's TCSADRAIN: this runs on a path that
        // must not block waiting for pending output to drain.
        let _ = termios::tcsetattr(&self.tty, SetArg::TCSANOW, &self.saved);
    }
}

/// One `readline` call, and where its answer goes.
struct Request {
    prompt: String,
    reply: oneshot::Sender<std::result::Result<String, ReadlineError>>,
}

pub(crate) struct EditorSource {
    requests: mpsc::Sender<Request>,
    // Declared last so it drops last: closing `requests` releases the reader
    // thread first, and restoring the terminal is the final thing that happens.
    _mode: TerminalModeGuard,
}

impl EditorSource {
    /// Build the editor, or `None` to fall back to plain line reads.
    ///
    /// Every failure is a fallback rather than an error: a session must not end
    /// because the terminal turned out to be unusual.
    pub(crate) fn try_new() -> Option<Self> {
        match Self::build() {
            Ok(source) => Some(source),
            Err(e) => {
                tracing::debug!(target: "outrig::repl", "no line editor, reading plain lines: {e}");
                None
            }
        }
    }

    fn build() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let term = std::env::var("TERM").ok();
        if !tty_capable(std::io::stdin().is_terminal(), term.as_deref()) {
            return Err(
                format!("stdin is not a terminal rustyline can drive (TERM={term:?})").into(),
            );
        }

        let lease = Arc::new(TerminalLease::new());
        let mode = TerminalModeGuard::capture(Arc::clone(&lease))?;

        let config = Config::builder()
            // Prompt and echo go to /dev/tty, leaving stdout for replies.
            .behavior(Behavior::PreferTerm)
            // `MemHistory::ignore` already drops empty lines and consecutive
            // duplicates, so this needs no filtering of its own.
            .auto_add_history(true)
            .history_ignore_space(true)
            .max_history_size(HISTORY_SIZE)?
            .build();

        let mut editor = DefaultEditor::with_config(config)?;

        // Taken before the editor moves to the reader thread: this is what lets
        // a container death reported from another task reach the terminal
        // without scribbling over a prompt being typed. See `super::notice`.
        let printer = editor.create_external_printer()?;

        let (requests, inbox) = mpsc::channel();
        let reader_lease = Arc::clone(&lease);
        std::thread::Builder::new()
            .name("outrig-readline".to_string())
            .spawn(move || reader_loop(editor, &inbox, &reader_lease))?;

        // Only now, with the editor safely owned by a live thread. Installing
        // before the spawn would leave the sink holding a printer whose editor
        // had been dropped on the error path -- and with it a `/dev/tty`
        // descriptor it does not own, which a later notice could write into
        // after the number had been reused.
        notice::install(Box::new(printer));

        Ok(Self {
            requests,
            _mode: mode,
        })
    }
}

/// The reader thread. Owns the editor -- and with it the history -- for as long
/// as the REPL is asking for lines. Detached, so tokio never waits on it: if
/// the REPL is cancelled mid-read it stays parked here until the process exits.
fn reader_loop(mut editor: DefaultEditor, inbox: &mpsc::Receiver<Request>, lease: &TerminalLease) {
    while let Ok(request) = inbox.recv() {
        let line = {
            // Held across `readline`, which is the whole span in which
            // rustyline has raw mode on, so the guard restoring the terminal
            // can tell this thread apart from an idle one.
            let Some(_reading) = lease.acquire() else {
                return;
            };
            editor.readline(&request.prompt)
        };
        if request.reply.send(line).is_err() {
            // Nobody is waiting: the REPL exited or was cancelled.
            break;
        }
    }
}

impl LineSource for EditorSource {
    /// Neither lent argument is used, and that is the whole point of this
    /// source. `out` goes unwritten because rustyline draws the prompt -- and
    /// the newline that ends it, on every `readline` return path -- straight to
    /// `/dev/tty`. `interrupt` is dropped unpolled because raw mode clears
    /// `ISIG`, so `Ctrl-C` never becomes a signal: it arrives as a keystroke
    /// and comes back below as `ReadlineError::Interrupted`. Racing a signal
    /// that cannot fire would only abandon a read still holding the terminal.
    async fn read_line<E, F>(
        &mut self,
        prompt: &str,
        _out: &mut E,
        _interrupt: F,
    ) -> Result<LineEvent>
    where
        E: AsyncWrite + Unpin,
        F: Future<Output = ()>,
    {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            prompt: prompt.to_string(),
            reply,
        };
        if self.requests.send(request).is_err() {
            return Ok(LineEvent::Eof);
        }
        match answer.await {
            Ok(line) => line_event(line),
            // The reader thread is gone. End the session rather than spin on a
            // prompt nothing will ever answer.
            Err(_) => Ok(LineEvent::Eof),
        }
    }
}

impl Drop for EditorSource {
    fn drop(&mut self) {
        // Before the fields: a printer whose editor has gone has nothing to
        // redraw, and out-of-band output belongs back on stderr from here.
        notice::clear();
    }
}

/// Translate one `readline` outcome. `Interrupted` and `Eof` are keystrokes
/// with REPL meaning, not failures; everything else ends the session.
fn line_event(line: std::result::Result<String, ReadlineError>) -> Result<LineEvent> {
    match line {
        Ok(line) => Ok(LineEvent::Line(line)),
        Err(ReadlineError::Eof) => Ok(LineEvent::Eof),
        Err(ReadlineError::Interrupted | ReadlineError::Signal(Signal::Interrupt)) => {
            Ok(LineEvent::Interrupted)
        }
        Err(ReadlineError::Io(e)) => Err(e.into()),
        // `ReadlineError` is `#[non_exhaustive]`, so this arm is required
        // rather than merely defensive.
        Err(other) => Err(std::io::Error::other(other).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that was already queued when cancellation landed must not start.
    /// `mpsc::recv` hands over a buffered request even after the sender is
    /// gone, so closing the channel does not cover this on its own -- the gate
    /// does, and it is what stops raw mode being re-entered after the terminal
    /// has been restored.
    #[test]
    fn a_stopped_lease_refuses_the_next_read() {
        let lease = TerminalLease::new();
        assert!(lease.acquire().is_some(), "a live lease must allow a read");

        lease.stopped.store(true, Ordering::SeqCst);
        assert!(lease.acquire().is_none(), "a stopped lease must refuse");
        assert!(lease.acquire().is_none(), "and keep refusing");
    }

    /// The guard signals through the same lease the reader consults, so a stop
    /// raised while another thread is between requests is seen by that thread.
    #[test]
    fn a_stop_raised_from_another_thread_is_observed() {
        let lease = Arc::new(TerminalLease::new());

        // Hold the lease as the reader does, then stop from elsewhere.
        let held = lease.acquire().expect("live lease");
        let stopper = {
            let lease = Arc::clone(&lease);
            std::thread::spawn(move || lease.stopped.store(true, Ordering::SeqCst))
        };
        stopper.join().expect("stopper thread");
        drop(held);

        assert!(
            lease.acquire().is_none(),
            "the next read must see the stop the guard raised",
        );
    }

    /// The case the fixed 50ms budget got wrong: a reader that has taken the
    /// lease but has not yet reached `enable_raw_mode` is committed to turning
    /// raw mode on, so restoring while it sits there is simply undone. The wait
    /// must last until raw mode is observable, however long the reader is
    /// descheduled for -- not until a deadline chosen in advance.
    #[test]
    fn a_lease_held_past_the_old_budget_is_waited_out_not_assumed() {
        const OLD_BUDGET: Duration = Duration::from_millis(50);

        let lease = Arc::new(TerminalLease::new());
        let held = lease.acquire().expect("live lease");
        let raw = Arc::new(AtomicBool::new(false));

        // Raw mode appears only well past the budget the guard used to give up
        // on, standing in for a reader the scheduler kept off the CPU.
        let flip_at = OLD_BUDGET * 3;
        let flipper = {
            let raw = Arc::clone(&raw);
            std::thread::spawn(move || {
                std::thread::sleep(flip_at);
                raw.store(true, Ordering::SeqCst);
            })
        };

        let started = Instant::now();
        let verdict = lease.await_quiescence(
            &|| raw.load(Ordering::SeqCst),
            // Generous: the point is that it returns on the state, not the bound.
            Duration::from_secs(5),
        );
        let waited = started.elapsed();

        flipper.join().expect("flipper thread");
        drop(held);

        assert!(
            matches!(verdict, Quiescence::Reading),
            "must resolve on observed raw mode, not on the clock",
        );
        assert!(
            waited >= flip_at,
            "gave up after {waited:?}, before raw mode was even entered",
        );
    }

    /// A released lease is the other definitive state: `readline` failing
    /// before it could enable raw mode leaves nothing to wait for.
    #[test]
    fn a_released_lease_settles_immediately() {
        let lease = TerminalLease::new();
        let verdict = lease.await_quiescence(&|| false, Duration::from_secs(5));
        assert!(matches!(verdict, Quiescence::Idle));
    }

    /// The backstop exists only so a `Drop` on the teardown path cannot hang.
    #[test]
    fn a_lease_that_never_settles_reports_rather_than_hanging() {
        let lease = TerminalLease::new();
        let _held = lease.acquire().expect("live lease");
        let verdict = lease.await_quiescence(&|| false, Duration::from_millis(20));
        assert!(matches!(verdict, Quiescence::Unresolved));
    }

    /// The gate is the whole defense against rustyline's `readline_direct`
    /// path, which writes the prompt to stdout and would put it in the file a
    /// redirected `outrig run` is capturing.
    #[test]
    fn unsupported_terminals_are_rejected_whatever_their_case() {
        for term in ["dumb", "DUMB", "Dumb", "cons25", "CONS25", "emacs", "Emacs"] {
            assert!(
                !tty_capable(true, Some(term)),
                "TERM={term} must not get an editor",
            );
        }
    }

    #[test]
    fn a_usable_terminal_is_accepted() {
        for term in [Some("xterm-256color"), Some("screen"), Some("dumber"), None] {
            assert!(tty_capable(true, term), "TERM={term:?} must get an editor");
        }
    }

    /// Piped stdin keeps the line-at-a-time reader, whatever TERM says: an
    /// editor here would read `/dev/tty` and ignore the piped script entirely.
    #[test]
    fn a_non_terminal_stdin_is_rejected() {
        assert!(!tty_capable(false, Some("xterm-256color")));
        assert!(!tty_capable(false, None));
    }
}
