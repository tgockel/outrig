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
use std::io::{IsTerminal, Write as _};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use nix::libc;
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use rustyline::error::{ReadlineError, Signal};
use rustyline::{Behavior, Config, DefaultEditor};
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;

use super::line_source::{LineEvent, LineSource};
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
/// The two stdin questions are outrig's own to ask and cannot be delegated:
/// under `PreferTerm` rustyline opened `/dev/tty` itself, so its internal
/// checks answer for that and say yes even when outrig's stdin is a pipe --
/// or some *other* terminal. `outrig run < /dev/pts/7` hands the REPL a tty
/// that is not the controlling one; the stream reader consumes it, as it
/// always has, where an editor on `/dev/tty` would read the user's own
/// terminal instead and ignore the one supplied.
fn tty_capable(stdin_is_tty: bool, stdin_is_controlling_tty: bool, term: Option<&str>) -> bool {
    stdin_is_tty
        && stdin_is_controlling_tty
        && !term.is_some_and(|term| {
            UNSUPPORTED_TERMS
                .iter()
                .any(|unsupported| unsupported.eq_ignore_ascii_case(term))
        })
}

/// Whether `fd` is this process's controlling terminal. `TIOCGSID` answers
/// for the controlling terminal only: for any other tty -- a `/dev/pts/N` the
/// shell redirected onto stdin -- Linux refuses with `ENOTTY`.
fn is_controlling_tty(fd: &impl AsRawFd) -> bool {
    // SAFETY: `tcgetsid` reads the terminal's session id and touches nothing
    // else; a descriptor that is not a tty just makes it fail.
    let session = unsafe { libc::tcgetsid(fd.as_raw_fd()) };
    session != -1
}

/// The escape sequence that turns bracketed paste off; rustyline turns it on
/// as part of taking the terminal for a read.
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// SIGINT's current disposition, read without changing it.
fn sigint_action() -> std::io::Result<libc::sigaction> {
    // SAFETY: a null `act` makes `sigaction` a pure query, and `old` is a
    // plain-data struct the call fills in whole before anyone reads it.
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut old) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(old)
}

/// Put a disposition `sigint_action` returned back in place.
fn install_sigint_action(action: &libc::sigaction) -> std::io::Result<()> {
    // SAFETY: `action` came out of `sigaction` for this process, so it is a
    // complete, valid disposition, and nothing else is passed in.
    if unsafe { libc::sigaction(libc::SIGINT, action, std::ptr::null_mut()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
    /// Inside `readline` with the terminal fully taken, which cannot happen a
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
    /// but has not yet finished taking the terminal is *committed* to doing
    /// so, and restoring in that window would simply be undone -- so the wait
    /// is for one of two states that can be observed rather than assumed, and
    /// both of which are reachable in microseconds:
    ///
    /// - the lease is free, so `stopped` has already stopped the reader, or
    /// - the reader has `settled`: it is inside `readline` with everything it
    ///   changes on the way in already changed, and cannot enter it again.
    ///
    /// Those are exhaustive after `acquire` succeeds, because `readline` either
    /// takes the terminal or fails and releases the lease. `settled` is
    /// injected so the ordering can be tested without a terminal.
    fn await_quiescence(&self, settled: &dyn Fn() -> bool, backstop: Duration) -> Quiescence {
        let deadline = Instant::now() + backstop;
        loop {
            if self.reading.try_lock().is_ok() {
                return Quiescence::Idle;
            }
            if settled() {
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
/// that call. Nothing restores it when the REPL future is dropped mid-read --
/// the session watcher saw the primary container die, or a SIGTERM or SIGHUP
/// ended the session -- because the `readline` that holds rustyline's guard is
/// parked on a detached thread and never returns. Teardown would run and the
/// process exit with the terminal still raw: the user would land back in a
/// shell with `ECHO` and `ICANON` cleared, and would have to type `stty sane`
/// blind.
///
/// It works because it lives in [`EditorSource`], on the REPL future's stack:
/// dropping that future runs it, ahead of teardown.
struct TerminalModeGuard {
    tty: File,
    saved: Termios,
    /// SIGINT's disposition before rustyline touched it -- tokio's handler,
    /// which `setup` installs ahead of the REPL. rustyline swaps its own in
    /// with raw `sigaction` for the span of every `readline` and swaps it back
    /// only when the call returns, so a parked read would leave tokio deaf to
    /// `Ctrl-C` for the rest of the process.
    saved_sigint: libc::sigaction,
    lease: Arc<TerminalLease>,
}

impl TerminalModeGuard {
    fn capture(lease: Arc<TerminalLease>) -> std::io::Result<Self> {
        let tty = File::options().read(true).write(true).open("/dev/tty")?;
        Self::capture_on(tty, lease)
    }

    /// [`capture`](Self::capture) over a terminal the caller opened, so a test
    /// can hand in one side of a pty.
    fn capture_on(tty: File, lease: Arc<TerminalLease>) -> std::io::Result<Self> {
        let saved = termios::tcgetattr(&tty)?;
        let saved_sigint = sigint_action()?;
        Ok(Self {
            tty,
            saved,
            saved_sigint,
            lease,
        })
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

    /// Whether rustyline has finished taking the terminal for a read. Its
    /// SIGINT handler goes in last -- after termios, after bracketed paste is
    /// switched on -- so seeing it means nothing else is still on its way.
    /// Raw mode alone is not enough: a reader observed between the two would
    /// switch paste back on and swap the handler in after this guard had
    /// undone both.
    fn reader_settled(&self) -> bool {
        self.is_raw()
            && sigint_action().is_ok_and(|now| now.sa_sigaction != self.saved_sigint.sa_sigaction)
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
        // this restore, with nothing left to undo it: rustyline's own restore
        // belongs to a `readline` that never returns.
        if let Quiescence::Unresolved = self
            .lease
            .await_quiescence(&|| self.reader_settled(), QUIESCENCE_BACKSTOP)
        {
            tracing::warn!(
                target: "outrig::repl",
                "line editor did not settle before teardown; the terminal may need `stty sane`",
            );
        }

        // Everything rustyline's own way out of `readline` would have done, in
        // its order, since the call holding that teardown never returns.
        // Termios first -- TCSANOW rather than rustyline's TCSADRAIN, because
        // this path must not block on pending output. Then bracketed paste
        // off, or the shell that comes next sees every paste wrapped in
        // `\e[200~`..`\e[201~` (bash and zsh switch it themselves; dash does
        // not). Then SIGINT back to the handler rustyline displaced, so a
        // `Ctrl-C` during teardown reaches `signals::unless_signaled` instead
        // of a pipe nobody reads. Each step is idempotent, so a reader that
        // was idle and had already done all three costs nothing here.
        let _ = termios::tcsetattr(&self.tty, SetArg::TCSANOW, &self.saved);
        let _ = (&self.tty).write_all(BRACKETED_PASTE_OFF);
        let _ = install_sigint_action(&self.saved_sigint);
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
        let stdin = std::io::stdin();
        if !tty_capable(
            stdin.is_terminal(),
            is_controlling_tty(&stdin),
            term.as_deref(),
        ) {
            return Err(format!(
                "stdin is not the controlling terminal, or not one rustyline can drive \
                 (TERM={term:?})"
            )
            .into());
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

        let editor = DefaultEditor::with_config(config)?;

        // Deliberately no `ExternalPrinter`. One would let a line raised from
        // another task -- a container death, a `tracing` warning -- redraw the
        // prompt instead of scribbling over it, but in rustyline 18.0.1 its
        // mere existence routes every key read through `select(2)` on the raw
        // descriptors without first draining the reader's own `BufReader`. So
        // whenever one `read` returns more than one key -- two keystrokes that
        // land together, a paste in a terminal without bracketed paste, a pty
        // driven by a script -- everything after the first byte sits
        // unprocessed until the next byte arrives. Measured on a pty: `slow\r`
        // echoed `s` and accepted the line only when a byte arrived two
        // seconds later; without the printer the same bytes are handled at
        // once. Prompt-safe notices are #439.
        let (requests, inbox) = mpsc::channel();
        let reader_lease = Arc::clone(&lease);
        std::thread::Builder::new()
            .name("outrig-readline".to_string())
            .spawn(move || reader_loop(editor, &inbox, &reader_lease))?;

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
                !tty_capable(true, true, Some(term)),
                "TERM={term} must not get an editor",
            );
        }
    }

    #[test]
    fn a_usable_terminal_is_accepted() {
        for term in [Some("xterm-256color"), Some("screen"), Some("dumber"), None] {
            assert!(
                tty_capable(true, true, term),
                "TERM={term:?} must get an editor"
            );
        }
    }

    /// Piped stdin keeps the line-at-a-time reader, whatever TERM says: an
    /// editor here would read `/dev/tty` and ignore the piped script entirely.
    #[test]
    fn a_non_terminal_stdin_is_rejected() {
        assert!(!tty_capable(false, false, Some("xterm-256color")));
        assert!(!tty_capable(false, false, None));
    }

    /// `outrig run < /dev/pts/7`: a terminal on stdin that is not the one the
    /// user is sitting at. An editor would read `/dev/tty` and ignore it.
    #[test]
    fn a_terminal_that_is_not_the_controlling_one_is_rejected() {
        assert!(!tty_capable(true, false, Some("xterm-256color")));
        assert!(!tty_capable(true, false, None));
    }

    /// The real check behind that argument, against a pty this test owns:
    /// neither side of it is this process's controlling terminal.
    #[test]
    fn a_foreign_pty_is_not_the_controlling_terminal() {
        let pty = nix::pty::openpty(None, None).expect("openpty");
        assert!(!is_controlling_tty(&pty.slave));
        assert!(!is_controlling_tty(&pty.master));
    }

    /// The guard undoes everything rustyline's own way out of `readline`
    /// would have, for a read that cancellation left parked: termios,
    /// bracketed paste, and the SIGINT handler rustyline swapped in. Driven
    /// against a pty of the test's own, with the reader "inside readline" for
    /// the whole of it, as it would be.
    #[test]
    fn a_cancelled_read_gets_its_terminal_handler_and_paste_mode_back() {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
        use std::io::Read;

        let pty = nix::pty::openpty(None, None).expect("openpty");
        let slave = File::from(pty.slave);
        let mut master = File::from(pty.master);
        let lease = Arc::new(TerminalLease::new());
        let before = sigint_action().expect("query SIGINT");
        let guard = TerminalModeGuard::capture_on(slave.try_clone().expect("dup"), lease.clone())
            .expect("capture");

        // rustyline's way in, in its order: the lease, raw mode, bracketed
        // paste on, then its own SIGINT handler.
        let reading = lease.acquire().expect("live lease");
        let mut raw = termios::tcgetattr(&slave).expect("tcgetattr");
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&slave, SetArg::TCSANOW, &raw).expect("raw mode");
        (&slave).write_all(b"\x1b[?2004h").expect("paste on");
        let foreign = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        // SAFETY: SIG_IGN dereferences nothing, and the guard dropped below
        // puts the previous disposition back.
        unsafe { sigaction(Signal::SIGINT, &foreign) }.expect("foreign handler");

        // The REPL future going away with the reader still parked.
        drop(guard);
        drop(reading);

        let after = termios::tcgetattr(&slave).expect("tcgetattr");
        assert!(
            after.local_flags.contains(LocalFlags::ICANON),
            "termios must be back to canonical",
        );
        let restored = sigint_action().expect("query SIGINT");
        assert_eq!(
            restored.sa_sigaction, before.sa_sigaction,
            "SIGINT must go back to the handler rustyline displaced",
        );
        // Exactly what the two writes put on the wire: paste on, paste off.
        let mut wire = [0u8; 16];
        master
            .read_exact(&mut wire)
            .expect("read the terminal side");
        assert_eq!(
            &wire[8..],
            BRACKETED_PASTE_OFF,
            "paste mode must be switched off"
        );
    }
}
