//! Lines for the user's terminal that nothing at the prompt asked for.
//!
//! The line editor owns the screen while a prompt is up: rustyline computes every
//! cursor movement from the layout it last drew. A line another task writes to
//! stderr in that span -- the session watcher's container-death notice, a
//! `tracing` warning, a subagent's tool-call trace, a retry notice -- lands on the
//! row rustyline believes it owns, and the next refresh paints from a screen that
//! no longer matches. On a prompt that wraps it can erase the notice outright.
//!
//! rustyline's `ExternalPrinter` is the designed answer and cannot be used: in
//! 18.0.1 merely creating one makes every key read go through `select(2)` without
//! looking at the bytes the reader has already buffered, so the second key of any
//! read that returned two sits unprocessed until a third byte arrives. The editor's
//! source comments have the measurements. So this is the printer-free route:
//!
//! - **No read in progress** -- the common case, including every line a primary
//!   turn raises beside the model's streamed reply -- the line is written to stderr
//!   before [`line`] returns, in the caller's order.
//! - **A read in progress** -- between [`reading`] and the drop of its guard -- the
//!   line is held, and written when the read ends: after rustyline has restored the
//!   terminal and ended the prompt's line, before the line typed is handed on and
//!   the next prompt is drawn. A warning raised mid-prompt is late, not garbled.
//!
//! Nothing here can lose a line: [`release`] flushes whatever a read that never
//! returned left held, and is the last thing the editor's teardown does.
//!
//! Everything that writes to the terminal on its own account while a session is
//! live goes through [`notice!`]. Writers that cannot overlap a prompt -- setup
//! and teardown progress, the subcommands that never start a REPL -- still use
//! `eprintln!`; `clippy.toml` disallows it in the modules that can.

use std::fmt;
use std::io::Write;
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// Write a line to the user's terminal without disturbing a live prompt. Takes
/// `format!`'s arguments; with none it writes an empty line, as `eprintln!()` does.
///
/// See the [module documentation](self).
macro_rules! notice {
    () => {
        $crate::notice::line(format_args!(""))
    };
    ($($arg:tt)+) => {
        $crate::notice::line(format_args!($($arg)+))
    };
}
pub(crate) use notice;

/// Where lines go, and whether they have to wait.
struct Hub {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Whether a read holds the terminal.
    reading: bool,
    /// Bytes raised during the read, in order.
    held: Vec<u8>,
    sink: Box<dyn Write + Send>,
}

impl Hub {
    fn new(sink: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                reading: false,
                held: Vec::new(),
                sink,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic elsewhere must not silence every later notice.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Write `bytes` now, or hold them until the read ends.
    ///
    /// The write happens under the lock, so a caller that finds no read running
    /// has its bytes out before it returns, and two callers cannot interleave
    /// within one line. A sink that fails loses the line: the terminal may be
    /// gone (SIGHUP), and `eprintln!`'s panic on that would be worse.
    fn write(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        if inner.reading {
            inner.held.extend_from_slice(bytes);
        } else {
            let _ = inner.sink.write_all(bytes);
            let _ = inner.sink.flush();
        }
    }

    fn begin_read(&self) {
        self.lock().reading = true;
    }

    /// Mark the read over and write out what it held.
    fn end_read(&self) {
        let mut inner = self.lock();
        inner.reading = false;
        if !inner.held.is_empty() {
            let held = std::mem::take(&mut inner.held);
            let _ = inner.sink.write_all(&held);
            let _ = inner.sink.flush();
        }
    }
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub::new(Box::new(std::io::stderr())));

/// Write one line (and its newline) to stderr, held back if a prompt is up.
/// Reach it through [`notice!`].
pub(crate) fn line(args: fmt::Arguments<'_>) {
    // One buffer, one `write`: the line cannot be split by another writer.
    HUB.write(format!("{args}\n").as_bytes());
}

/// Held while a prompt owns the terminal. Dropping it writes out the lines that
/// were raised meanwhile.
#[must_use = "the guard is what marks the read as running"]
pub(crate) struct Reading(());

/// A read is starting: hold notices until the returned guard drops.
pub(crate) fn reading() -> Reading {
    HUB.begin_read();
    Reading(())
}

impl Drop for Reading {
    fn drop(&mut self) {
        HUB.end_read();
    }
}

/// No read holds the terminal any more, whether or not the reader thread has
/// noticed: write out what is held and let later lines through. The editor's
/// teardown calls this once it has restored the terminal, so a notice raised
/// during a read that could not be ended is not lost with the process.
pub(crate) fn release() {
    HUB.end_read();
}

/// `tracing`'s writer, so a record shares the discipline above. Each record is
/// collected and handed over whole when the formatter is done with it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TracingWriter;

pub(crate) struct Record(Vec<u8>);

impl<'a> MakeWriter<'a> for TracingWriter {
    type Writer = Record;

    fn make_writer(&'a self) -> Record {
        Record(Vec::new())
    }
}

impl Write for Record {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            HUB.write(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A sink the test can read back.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn hub() -> (Hub, Captured) {
        let sink = Captured::default();
        (Hub::new(Box::new(sink.clone())), sink)
    }

    /// With no read running a line is out before the call returns, so it cannot
    /// reorder against a reply streamed to stdout by the same caller.
    #[test]
    fn a_line_with_no_read_running_is_written_before_the_call_returns() {
        let (hub, sink) = hub();
        hub.write(b"first\n");
        assert_eq!(sink.text(), "first\n");
        hub.write(b"second\n");
        assert_eq!(sink.text(), "first\nsecond\n");
    }

    /// The point of the module: nothing reaches the terminal while the prompt is
    /// up, and all of it does, in order, when the read ends.
    #[test]
    fn lines_raised_during_a_read_wait_for_it_to_end() {
        let (hub, sink) = hub();
        hub.begin_read();
        hub.write(b"one\n");
        hub.write(b"two\n");
        assert_eq!(sink.text(), "", "a held line must not touch the terminal");

        hub.end_read();
        assert_eq!(sink.text(), "one\ntwo\n");

        hub.write(b"three\n");
        assert_eq!(
            sink.text(),
            "one\ntwo\nthree\n",
            "lines go straight out again"
        );
    }

    /// `release` is the teardown path for a read that never returned: its held
    /// lines are written, and the reader ending later finds nothing to repeat.
    #[test]
    fn release_writes_held_lines_once() {
        let (hub, sink) = hub();
        hub.begin_read();
        hub.write(b"container died\n");
        hub.end_read();
        hub.end_read();
        assert_eq!(sink.text(), "container died\n");
    }

    /// A sink that fails drops the line and does not panic.
    #[test]
    fn a_failing_sink_does_not_panic() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let hub = Hub::new(Box::new(Broken));
        hub.write(b"lost\n");
        hub.begin_read();
        hub.write(b"also lost\n");
        hub.end_read();
    }
}
