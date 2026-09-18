//! Out-of-band lines that must not scribble over an active prompt.
//!
//! Anything written to the terminal from outside the line editor lands under a
//! line rustyline believes it owns. Its next refresh then computes cursor
//! motion from a screen that no longer matches, and the prompt garbles until
//! Enter -- while the text about to be sent stops being the text on display.
//! The buffer is never wrong; what the user can read is.
//!
//! [`print`] routes such a line through rustyline's `ExternalPrinter` when one
//! is installed, which erases the prompt, writes, and redraws. The printer
//! decides for itself whether a read is actually in progress: it holds the same
//! `raw_mode` flag the terminal sets, and writes straight out when the flag is
//! clear, so a notice raised between turns is not held back.
//!
//! Two callers, both of which can fire while a prompt is up: the session
//! watcher's container-death lines, and -- via [`make_writer`] -- every
//! `tracing` record, whose default filter is `info` and so emits `warn!`
//! without anyone opting in.
//!
//! Plain `eprintln!` is kept for the case where it cannot do damage *and*
//! carries information the printer would lose: with stderr redirected, nothing
//! reaches the terminal to corrupt, and writing to the tty instead would drop
//! the line from the file the user is capturing.
//!
//! # Why a dispatcher thread
//!
//! `ExternalPrinter::print` is not a fire-and-forget call. Under raw mode it
//! pushes onto a `sync_channel(1)` that only drains while a read is running,
//! and rustyline's reader prefers a keystroke over a pending print, so a second
//! notice can find the queue still full and **block**. Blocking the caller is
//! not an option: the watcher runs on the binary's current-thread runtime, so a
//! blocked `print` stops the very task that would start the next read and drain
//! the queue -- a deadlock in which neither side can move. Producers therefore
//! hand the message to an unbounded queue and return; only the dispatcher ever
//! blocks, and the cost of it doing so is a late notice rather than a frozen
//! session.

use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rustyline::ExternalPrinter;

type Printer = Box<dyn ExternalPrinter + Send>;

/// Bound on how long [`clear`] waits for a print already under way.
///
/// Only the `!raw_mode` branch of `ExternalPrinter::print` touches the
/// terminal descriptor, and that branch is a single non-blocking `write`.
/// Exhausting this bound therefore means the dispatcher is in the *other*
/// branch, which queues onto rustyline's own channel and writes to a pipe it
/// owns -- nothing there outlives the editor.
const DRAIN_BACKSTOP: Duration = Duration::from_millis(250);

/// Serializes the dispatcher's use of the printer against the editor going
/// away, so a late notice cannot write through a descriptor the editor has
/// already closed.
struct PrintGuard {
    printing: Mutex<()>,
    stopped: AtomicBool,
}

struct Sink {
    sender: mpsc::Sender<String>,
    guard: Arc<PrintGuard>,
}

/// Installed while a rustyline editor owns the terminal, `None` otherwise --
/// which covers piped stdin, `TERM=dumb`, and every moment before the REPL
/// starts or after it ends. Never held across a send or a wait.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Take ownership of `printer` on a thread of its own and start accepting
/// notices. A failure to spawn leaves the sink empty, which is not an error:
/// [`print`] simply keeps using stderr.
pub(crate) fn install(printer: Printer) {
    let (sender, inbox) = mpsc::channel::<String>();
    let guard = Arc::new(PrintGuard {
        printing: Mutex::new(()),
        stopped: AtomicBool::new(false),
    });
    let dispatch_guard = Arc::clone(&guard);
    let spawned = std::thread::Builder::new()
        .name("outrig-notice".to_string())
        .spawn(move || dispatch(printer, &inbox, &dispatch_guard));
    if spawned.is_err() {
        return;
    }
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(Sink { sender, guard });
    }
}

/// Stop routing through the editor, and do not return until the printer is
/// out of use.
///
/// Called from `EditorSource::drop`, which runs *before* that struct's fields
/// -- so the reader thread, and with it the editor that owns `/dev/tty`, is
/// still alive here. That is the window in which the dispatcher has to be shut
/// down: closing the channel alone would not do, because `mpsc::recv` keeps
/// handing over buffered messages after the sender is gone, and a message
/// already dequeued is already past any check. So `stopped` cancels what is
/// queued and the wait covers what is in flight, leaving nothing that could
/// write through the descriptor once the editor closes it.
///
/// Everything blocking happens with `SINK` released.
pub(crate) fn clear() {
    let sink = SINK.lock().ok().and_then(|mut slot| slot.take());
    let Some(Sink { sender, guard }) = sink else {
        return;
    };
    drop(sender);
    guard.stopped.store(true, Ordering::SeqCst);

    let deadline = Instant::now() + DRAIN_BACKSTOP;
    while guard.printing.try_lock().is_err() && Instant::now() < deadline {
        std::thread::yield_now();
    }
}

fn dispatch(mut printer: Printer, inbox: &mpsc::Receiver<String>, guard: &PrintGuard) {
    while let Ok(msg) = inbox.recv() {
        // Taken before the `stopped` check, so `clear` cannot conclude the
        // printer is idle while this iteration is on its way into it.
        let _printing = guard
            .printing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.stopped.load(Ordering::SeqCst) {
            break;
        }
        // Blocking here is expected and harmless: the queue drains on the next
        // read. A printer that errors is not retried -- dropping the receiver
        // sends every later notice back to stderr.
        if printer.print(msg).is_err() {
            break;
        }
    }
}

/// Write one line of out-of-band output, prompt-safely where that matters.
///
/// Falls back to stderr whenever the sink is absent, gone, or would take the
/// line away from a redirect that wants it.
pub fn print(msg: &str) {
    print_when(std::io::stderr().is_terminal(), msg);
}

/// [`print`] with the terminal question already answered, so the queueing
/// behavior can be tested where stderr is a pipe.
fn print_when(stderr_is_terminal: bool, msg: &str) {
    if stderr_is_terminal {
        // Cloned out, so nothing holds the lock across the send.
        let sender = SINK
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|sink| sink.sender.clone()));
        if let Some(sender) = sender
            && sender.send(format!("{msg}\n")).is_ok()
        {
            return;
        }
    }
    eprintln!("{msg}");
}

/// One `tracing` record's worth of bytes, forwarded to [`print`] when complete.
///
/// The `fmt` layer writes a record to a fresh writer and drops it, so the whole
/// record is accumulated and emitted once rather than per-chunk -- a partial
/// write handed to the printer would be redrawn as its own line.
pub struct NoticeWriter(Vec<u8>);

/// `MakeWriter` for `tracing_subscriber`, in place of `std::io::stderr`.
pub fn make_writer() -> NoticeWriter {
    NoticeWriter(Vec::new())
}

impl NoticeWriter {
    fn emit(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.0);
        if let Ok(text) = String::from_utf8(buf) {
            print(text.trim_end_matches('\n'));
        }
    }
}

impl io::Write for NoticeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit();
        Ok(())
    }
}

impl Drop for NoticeWriter {
    fn drop(&mut self) {
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SINK` is process-wide, so these must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records what it printed, holding each call open for `hold` and
    /// announcing on `entered` the moment it is inside one.
    struct SlowPrinter {
        entered: mpsc::Sender<()>,
        printed: Arc<Mutex<Vec<String>>>,
        hold: Duration,
    }

    impl ExternalPrinter for SlowPrinter {
        fn print(&mut self, msg: String) -> rustyline::Result<()> {
            let _ = self.entered.send(());
            std::thread::sleep(self.hold);
            self.printed.lock().unwrap().push(msg);
            Ok(())
        }
    }

    /// The editor that owns `/dev/tty` is dropped just after `clear` returns,
    /// and rustyline's printer keeps that descriptor *number* without owning
    /// it. So `clear` has to leave nothing that could still call `print`:
    /// neither an in-flight call (waited out here) nor a queued one (cancelled
    /// here) -- a late write would otherwise land in whatever file teardown
    /// had since been given that number.
    #[test]
    fn clear_waits_out_an_in_flight_print_and_cancels_the_queued_rest() {
        let _serial = serial();
        const HOLD: Duration = Duration::from_millis(150);

        let (entered, is_printing) = mpsc::channel();
        let printed = Arc::new(Mutex::new(Vec::new()));
        install(Box::new(SlowPrinter {
            entered,
            printed: Arc::clone(&printed),
            hold: HOLD,
        }));

        for msg in ["first", "second", "third"] {
            print_when(true, msg);
        }
        // Pause the dispatcher inside `print`, which is the window that matters.
        is_printing
            .recv()
            .expect("dispatcher must reach the printer");

        let started = Instant::now();
        clear();
        let waited = started.elapsed();

        assert!(
            waited >= HOLD / 2,
            "clear returned after {waited:?} with a print still running",
        );

        // Nothing queued may be printed after the editor is considered gone.
        std::thread::sleep(HOLD * 2);
        let printed = printed.lock().unwrap().clone();
        assert_eq!(
            printed,
            vec!["first\n".to_string()],
            "only the in-flight notice may complete",
        );
    }

    /// A printer wedged the way rustyline's real one can be: its queue holds
    /// one message and drains only while a read is running.
    struct WedgedPrinter(mpsc::Receiver<()>);

    impl ExternalPrinter for WedgedPrinter {
        fn print(&mut self, _msg: String) -> rustyline::Result<()> {
            let _ = self.0.recv();
            Ok(())
        }
    }

    /// The deadlock this indirection exists to prevent: the watcher runs on the
    /// binary's current-thread runtime, so a `print` that blocked there would
    /// stop the task that starts the next read -- the only thing that drains
    /// the queue it is blocked on. Producers must hand off and return.
    #[test]
    fn a_wedged_printer_does_not_block_the_caller() {
        let _serial = serial();
        // Precondition: the fixture really does wedge. Without this the timing
        // below would also pass against a printer that simply returned.
        let (unwedge, wedged) = mpsc::channel();
        let mut direct = WedgedPrinter(wedged);
        let held = std::thread::spawn(move || direct.print("held".to_string()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!held.is_finished(), "the fixture must actually block");
        drop(unwedge);
        let _ = held.join().expect("fixture thread");

        let (release, blocked) = mpsc::channel();
        install(Box::new(WedgedPrinter(blocked)));

        let started = Instant::now();
        for i in 0..16 {
            print_when(true, &format!("notice {i}"));
        }
        let elapsed = started.elapsed();

        drop(release);
        clear();

        assert!(
            elapsed < Duration::from_millis(250),
            "producers waited {elapsed:?} on a printer that never returned",
        );
    }
}
