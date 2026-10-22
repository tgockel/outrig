//! Where `run-new`'s output is written: on threads of their own, so a terminal
//! that is slow or has stopped holds up what is shown and nothing else.
//!
//! [`converse`](super::converse)'s loop drives the round, reads what the user
//! types and takes what the agent sends, all from one task. A write that
//! waited in that task would hold every one of them up behind the terminal --
//! the round included, which is polled only when the loop gets to it -- so the
//! loop only queues output, and the threads here write it.
//!
//! There are two, one for each stream, so a stdout nobody is reading cannot
//! hold up stderr, where the model's text and the prompt go. They are threads
//! rather than tasks for the reason stdin is read on one: tokio runs a write
//! to its `Stdout` on its blocking pool, and the runtime does not shut down
//! while the pool has a write outstanding, which a stopped pipe never lets
//! finish.
//!
//! - stderr's queue is unbounded. What goes on it is a round's text, the
//!   Python it ran, notes and the prompt, produced at the pace of model calls
//!   and typing. It is one queue, so each is written in the order it was
//!   queued.
//! - stdout's holds [`OUTBOX`] messages, and the loop takes one of the agent's
//!   only when there is room for it ([`Output::room`]). The agent's code runs
//!   at most 16 messages ahead of what the loop has taken, so the host holds
//!   at most 16 + `OUTBOX` + 1 of them, the last being the one being written.
//!   `doc/reference/cli.md` states that sum.
//!   With no room the loop takes nothing, and waits on nothing: the agent's
//!   code waits in `send`, as it did when the loop itself waited on the
//!   terminal.
//!
//! A message that arrives while the prompt is showing is framed: the prompt's
//! line is ended before it, and the prompt drawn again after. The frame is on
//! stderr, so the stdout thread asks for it on stderr's queue, where it
//! follows what was queued there before, and waits for the first part to be
//! written before it writes the message. Stderr's queue never waits on
//! stdout's.
//!
//! When stdout and stderr are one destination -- `2>&1`, or one terminal -- each
//! write holds a lock, so a line too long for the destination to take at once
//! is not cut by the other stream's. With two destinations the threads do not
//! wait on each other.
//!
//! With one destination, a write to stderr waits behind the one stdout is in,
//! so once the user has asked to go and stdout is abandoned, [`Output::settle`]
//! waits on stderr no more than on stdout. The caller says what the session
//! closes with through stderr's queue too, which writes it in turn and leaves
//! the caller free to stop the session whether or not it could be written;
//! [`Output::finish`] then waits for it only as long as the caller allows.
//! That is not the conversation's output, so a write of it that fails is not
//! reported through [`Failures`]; the caller reads what the conversation's did
//! once the stop is done.
//!
//! Whether a message is framed is decided when the loop takes it, and the
//! frame is applied when it is written. Behind a terminal that has fallen
//! behind, the prompt may have moved on by then, which costs a stray or a
//! missing line break around the message and never the message.

use std::future::Future;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) const PROMPT: &[u8] = b"> ";

/// How many of the agent's messages wait for stdout, beyond the one being
/// written.
pub(super) const OUTBOX: usize = 4;

enum ToStderr {
    /// Written as it is.
    Text(Vec<u8>),
    /// Written as it is, and a failure to is not reported.
    BestEffort(Vec<u8>),
    /// Answered once everything queued before it has been written. Nothing
    /// queued after it is, since the thread ends.
    Finish(oneshot::Sender<()>),
    /// Answered once everything queued before it has been written.
    Written(oneshot::Sender<()>),
}

/// Held across each write when stdout and stderr are one destination.
type Serial = Option<Arc<Mutex<()>>>;

enum ToStdout {
    /// A message of the agent's, framed if it came while the prompt showed.
    Message { text: String, framed: bool },
    /// Answered once everything queued before it has been written.
    Written(oneshot::Sender<()>),
}

/// The queues in front of the two threads. Everything on them is written in
/// the order it was queued, per stream.
pub(super) struct Output {
    stderr: UnboundedSender<ToStderr>,
    stdout: mpsc::Sender<ToStdout>,
    /// Cancelled when the user has asked to go: stdout writes nothing more.
    abandoned: CancellationToken,
    /// Cancelled when the last wait was cut short: stderr takes nothing more.
    closed: CancellationToken,
    /// Whether the two are one destination.
    shared: bool,
}

/// The writes that failed. Each thread stops at its first.
pub(super) type Failures = UnboundedReceiver<io::Error>;

/// A place in stdout's queue, held for a message the loop has yet to take.
pub(super) struct Room<'a>(mpsc::Permit<'a, ToStdout>);

impl Output {
    /// The process's own stdout and stderr.
    pub(super) fn stdio() -> io::Result<(Self, Failures)> {
        let shared = same_destination(&io::stdout(), &io::stderr());
        Self::spawn(io::stdout(), io::stderr(), shared)
    }

    /// With `shared`, the two are one destination, and are written one at a
    /// time.
    fn spawn(
        stdout: impl Write + Send + 'static,
        stderr: impl Write + Send + 'static,
        shared: bool,
    ) -> io::Result<(Self, Failures)> {
        let (to_stderr, stderr_queue) = mpsc::unbounded_channel();
        let (to_stdout, stdout_queue) = mpsc::channel(OUTBOX);
        let (failed, failures) = mpsc::unbounded_channel();
        let (abandoned, closed) = (CancellationToken::new(), CancellationToken::new());
        let serial: Serial = shared.then(Arc::default);
        std::thread::Builder::new().name("stderr".to_string()).spawn({
            let (closed, failed, serial) = (closed.clone(), failed.clone(), serial.clone());
            move || serve_stderr(stderr, stderr_queue, &closed, &serial, &failed)
        })?;
        std::thread::Builder::new().name("stdout".to_string()).spawn({
            let (to_stderr, abandoned) = (to_stderr.clone(), abandoned.clone());
            move || serve_stdout(stdout, stdout_queue, &to_stderr, &abandoned, &serial, &failed)
        })?;
        let output = Self {
            stderr: to_stderr,
            stdout: to_stdout,
            abandoned,
            closed,
            shared,
        };
        Ok((output, failures))
    }

    /// `text` on stderr, as it is.
    pub(super) fn text(&self, text: impl Into<Vec<u8>>) {
        // A thread that has gone has said why through `Failures`.
        let _ = self.stderr.send(ToStderr::Text(text.into()));
    }

    /// `text` on stderr, as it is, for what the session says as it closes. A
    /// write of it that fails says nothing of the conversation, so it is not
    /// reported through [`Failures`].
    pub(super) fn text_best_effort(&self, text: impl Into<Vec<u8>>) {
        let _ = self.stderr.send(ToStderr::BestEffort(text.into()));
    }

    /// `text` as a line on stderr.
    pub(super) fn line(&self, text: &str) {
        self.text(lined(text));
    }

    /// Room for one more of the agent's messages on stdout's queue, waiting
    /// until there is. `None` once the stdout thread has stopped. Cancel-safe.
    pub(super) async fn room(&self) -> Option<Room<'_>> {
        self.stdout.reserve().await.ok().map(Room)
    }

    /// Stop writing to stdout, since the user has asked to go: a message in
    /// the queue is dropped, and the thread ends once the write it is in, if
    /// any, does. It may never, to a stdout that has stopped. [`Output::settle`]
    /// then does not wait on stdout.
    pub(super) fn abandon(&self) {
        self.abandoned.cancel();
    }

    /// Wait for what is queued for stdout to be written, or for the thread to
    /// stop.
    async fn drain_stdout(&self) {
        let (done, written) = oneshot::channel();
        if self.stdout.send(ToStdout::Written(done)).await.is_ok() {
            let _ = written.await;
        }
    }

    /// Wait for what is queued for stderr to be written, or for the thread to
    /// stop; with `finish`, then stop it.
    async fn drain_stderr(&self, finish: bool) {
        let (done, written) = oneshot::channel();
        let asked = if finish {
            ToStderr::Finish(done)
        } else {
            ToStderr::Written(done)
        };
        if self.stderr.send(asked).is_ok() {
            let _ = written.await;
        }
    }

    /// Wait for what is queued to be written: stderr's, and stdout's too
    /// unless it has been abandoned, and then not stderr's either when the two
    /// are one destination. When `interrupted` completes first -- the user has
    /// pressed Ctrl-C on a terminal that has stopped -- stop waiting, and
    /// abandon stdout, so that settling again does not wait on it either.
    pub(super) async fn settle(&self, interrupted: impl Future) {
        self.wait(false, interrupted).await;
    }

    /// [`Output::settle`] for the last time: it waits for stderr's queue
    /// whatever has become of stdout, and stderr's thread then ends. If
    /// `interrupted` completes first -- the caller's bound on the wait -- the
    /// thread ends after the write it is in, if any, and what is still queued
    /// is dropped.
    pub(super) async fn finish(&self, interrupted: impl Future) {
        self.wait(true, interrupted).await;
    }

    async fn wait(&self, finish: bool, interrupted: impl Future) {
        let written = async {
            let abandoned = self.abandoned.is_cancelled();
            if !abandoned {
                self.drain_stdout().await;
            }
            if finish || !(abandoned && self.shared) {
                self.drain_stderr(finish).await;
            }
        };
        tokio::select! {
            () = written => {}
            _ = interrupted => {
                self.abandon();
                if finish {
                    self.closed.cancel();
                }
            }
        }
    }
}

impl Room<'_> {
    pub(super) fn send(self, text: String, framed: bool) {
        self.0.send(ToStdout::Message { text, framed });
    }
}

/// `text` ended with a newline, as one buffer so it is written whole.
fn lined(text: &str) -> Vec<u8> {
    let mut line = Vec::with_capacity(text.len() + 1);
    line.extend_from_slice(text.as_bytes());
    if !text.ends_with('\n') {
        line.push(b'\n');
    }
    line
}

/// Whether `a` and `b` are one file, pipe or terminal.
pub(super) fn same_destination(a: &impl AsFd, b: &impl AsFd) -> bool {
    match (nix::sys::stat::fstat(a), nix::sys::stat::fstat(b)) {
        (Ok(a), Ok(b)) => (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino),
        _ => false,
    }
}

fn write_whole(out: &mut impl Write, bytes: &[u8], serial: &Serial) -> io::Result<()> {
    // Nothing a panic leaves behind is worth refusing a write for.
    let _held = serial
        .as_ref()
        .map(|lock| lock.lock().unwrap_or_else(PoisonError::into_inner));
    out.write_all(bytes)?;
    out.flush()
}

fn serve_stderr(
    mut out: impl Write,
    mut queue: UnboundedReceiver<ToStderr>,
    closed: &CancellationToken,
    serial: &Serial,
    failed: &UnboundedSender<io::Error>,
) {
    while let Some(item) = queue.blocking_recv() {
        if closed.is_cancelled() {
            return;
        }
        let written = match item {
            ToStderr::Text(text) => write_whole(&mut out, &text, serial),
            ToStderr::BestEffort(text) => {
                if write_whole(&mut out, &text, serial).is_err() {
                    return;
                }
                Ok(())
            }
            ToStderr::Written(done) => {
                let _ = done.send(());
                Ok(())
            }
            ToStderr::Finish(done) => {
                let _ = done.send(());
                return;
            }
        };
        if let Err(e) = written {
            let _ = failed.send(e);
            return;
        }
    }
}

fn serve_stdout(
    mut out: impl Write,
    mut queue: mpsc::Receiver<ToStdout>,
    stderr: &UnboundedSender<ToStderr>,
    abandoned: &CancellationToken,
    serial: &Serial,
    failed: &UnboundedSender<io::Error>,
) {
    while let Some(item) = queue.blocking_recv() {
        if abandoned.is_cancelled() {
            return;
        }
        let (text, framed) = match item {
            ToStdout::Message { text, framed } => (text, framed),
            ToStdout::Written(done) => {
                let _ = done.send(());
                continue;
            }
        };
        if framed {
            // Ends the prompt's line, and only then may the message follow.
            let (done, written) = oneshot::channel();
            let asked = stderr.send(ToStderr::Text(b"\n".to_vec())).is_ok()
                && stderr.send(ToStderr::Written(done)).is_ok();
            if !asked || written.blocking_recv().is_err() {
                return;
            }
        }
        if let Err(e) = write_whole(&mut out, &lined(&text), serial) {
            let _ = failed.send(e);
            return;
        }
        // Not once the user has asked to go: the prompt would land after what
        // the session says as it closes.
        if framed && !abandoned.is_cancelled() {
            let _ = stderr.send(ToStderr::Text(PROMPT.to_vec()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use futures_util::FutureExt;

    use super::*;

    /// What the writers wrote, each write as `<tag>:<text>`, in the order they
    /// happened across both.
    type Log = Arc<Mutex<Vec<String>>>;

    /// A writer that records what it is given and can be held shut: a write to
    /// a shut one waits, as a write to a stopped pipe does.
    #[derive(Clone)]
    struct Gate {
        tag: &'static str,
        log: Log,
        state: Arc<(Mutex<GateState>, Condvar)>,
    }

    struct GateState {
        open: bool,
        entered: usize,
    }

    impl Gate {
        fn new(tag: &'static str, log: &Log, open: bool) -> Self {
            Self {
                tag,
                log: Arc::clone(log),
                state: Arc::new((Mutex::new(GateState { open, entered: 0 }), Condvar::new())),
            }
        }

        fn shut(tag: &'static str, log: &Log) -> Self {
            Self::new(tag, log, false)
        }

        fn open(tag: &'static str, log: &Log) -> Self {
            Self::new(tag, log, true)
        }

        fn let_through(&self) {
            self.state.0.lock().expect("unpoisoned").open = true;
            self.state.1.notify_all();
        }

        /// Whether `writes` writes have started within `within`.
        fn entered(&self, writes: usize, within: Duration) -> bool {
            let (state, changed) = &*self.state;
            let state = state.lock().expect("unpoisoned");
            let (state, _) = changed
                .wait_timeout_while(state, within, |s| s.entered < writes)
                .expect("unpoisoned");
            state.entered >= writes
        }

        /// Wait for the thread to be inside its `writes`th write.
        fn reached(&self, writes: usize) {
            assert!(self.entered(writes, Duration::from_secs(10)), "no write {writes}");
        }
    }

    impl Write for Gate {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("unpoisoned");
            state.entered += 1;
            changed.notify_all();
            while !state.open {
                state = changed.wait(state).expect("unpoisoned");
            }
            drop(state);
            let text = String::from_utf8_lossy(buf);
            self.log.lock().expect("unpoisoned").push(format!("{}:{text}", self.tag));
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every write fails, as one to a closed pipe does.
    struct Broken(io::ErrorKind);

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn start(stdout: &Gate, stderr: &Gate) -> (Output, Failures) {
        Output::spawn(stdout.clone(), stderr.clone(), false).expect("threads")
    }

    /// [`start`], with the two writers one destination.
    fn start_shared(stdout: &Gate, stderr: &Gate) -> (Output, Failures) {
        Output::spawn(stdout.clone(), stderr.clone(), true).expect("threads")
    }

    fn written(log: &Log) -> Vec<String> {
        log.lock().expect("unpoisoned").clone()
    }

    /// With stdout stuck in a write, stderr is still written (#471). A single
    /// queue for both streams would hold the line behind the message.
    #[tokio::test]
    async fn stderr_is_written_while_stdout_is_stuck() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m".into(), false);
        stdout.reached(1);
        out.line("reply");
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["E:reply\n"]);

        stdout.let_through();
        out.drain_stdout().await;
        assert_eq!(written(&log), ["E:reply\n", "O:m\n"]);
    }

    /// Stdout holds `OUTBOX` messages beyond the one being written, and then
    /// makes the loop wait for room rather than queueing more without bound.
    #[tokio::test]
    async fn stdout_holds_outbox_messages_and_then_has_no_room() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m0".into(), false);
        stdout.reached(1);
        for i in 1..=OUTBOX {
            out.room().await.expect("room").send(format!("m{i}"), false);
        }
        assert!(out.room().now_or_never().is_none(), "room past the bound");

        stdout.let_through();
        assert!(out.room().await.is_some(), "room again once stdout drains");
    }

    /// A message at the prompt follows the line the prompt is on being ended,
    /// which waits behind what stderr was writing, and the prompt is drawn
    /// again after it.
    #[tokio::test]
    async fn a_message_at_the_prompt_is_framed_in_order() {
        let log = Log::default();
        let (stdout, stderr) = (Gate::open("O", &log), Gate::shut("E", &log));
        let (out, _failures) = start(&stdout, &stderr);

        out.text(PROMPT);
        stderr.reached(1);
        out.room().await.expect("room").send("m".into(), true);
        assert!(
            !stdout.entered(1, Duration::from_millis(100)),
            "the message went ahead of the end of the prompt's line"
        );

        stderr.let_through();
        out.drain_stdout().await;
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["E:> ", "E:\n", "O:m\n", "E:> "]);
    }

    #[tokio::test]
    async fn a_message_between_rounds_is_written_alone() {
        let log = Log::default();
        let (out, _failures) = start(&Gate::open("O", &log), &Gate::open("E", &log));

        out.room().await.expect("room").send("m\n".into(), false);
        out.drain_stdout().await;
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["O:m\n"]);
    }

    /// A write that fails is reported while the loop is idle, as it reached
    /// the loop before when the loop made the write.
    #[tokio::test]
    async fn a_failed_write_is_reported() {
        let log = Log::default();
        let (out, mut failures) = Output::spawn(
            Broken(io::ErrorKind::BrokenPipe),
            Gate::open("E", &log),
            false,
        )
        .expect("threads");

        out.room().await.expect("room").send("m".into(), false);
        let failed = failures.recv().await.expect("the write failed");
        assert_eq!(failed.kind(), io::ErrorKind::BrokenPipe);
    }

    /// What the session says as it closes is best effort: a write of it that
    /// fails is not a failure of the conversation, which is what [`Failures`]
    /// reports.
    #[tokio::test]
    async fn a_failed_closing_write_is_not_reported() {
        let log = Log::default();
        let (out, mut failures) = Output::spawn(
            Gate::open("O", &log),
            Broken(io::ErrorKind::BrokenPipe),
            false,
        )
        .expect("threads");

        out.text_best_effort("closing\n");
        out.finish(pending::<()>()).await;
        assert!(failures.try_recv().is_err(), "the closing write was reported");

        // The conversation's own write on the same destination is.
        let (out, mut failures) = Output::spawn(
            Gate::open("O", &log),
            Broken(io::ErrorKind::BrokenPipe),
            false,
        )
        .expect("threads");
        out.text("note\n");
        let failed = failures.recv().await.expect("the write failed");
        assert_eq!(failed.kind(), io::ErrorKind::BrokenPipe);
    }

    /// Draining waits for the backlog, in order.
    #[tokio::test]
    async fn draining_waits_for_what_is_queued() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m0".into(), false);
        stdout.reached(1);
        out.room().await.expect("room").send("m1".into(), false);
        let mut drain = Box::pin(out.drain_stdout());
        assert!(drain.as_mut().now_or_never().is_none(), "drained while stuck");

        stdout.let_through();
        drain.await;
        assert_eq!(written(&log), ["O:m0\n", "O:m1\n"]);
    }

    /// Settling waits for both queues.
    #[tokio::test]
    async fn settling_waits_for_both_streams() {
        let log = Log::default();
        let (stdout, stderr) = (Gate::shut("O", &log), Gate::shut("E", &log));
        let (out, _failures) = start(&stdout, &stderr);

        out.room().await.expect("room").send("m".into(), false);
        out.line("note");
        let mut settle = Box::pin(out.settle(pending::<()>()));
        assert!(settle.as_mut().now_or_never().is_none(), "settled while stuck");

        stdout.let_through();
        stderr.let_through();
        settle.await;
        let log = written(&log);
        assert!(log.contains(&"O:m\n".to_string()) && log.contains(&"E:note\n".to_string()));
    }

    /// Once stdout is abandoned, settling waits for stderr alone, however
    /// stuck stdout is.
    #[tokio::test]
    async fn settling_after_abandoning_leaves_stdout_stuck() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m".into(), false);
        stdout.reached(1);
        out.line("note");
        out.abandon();
        tokio::time::timeout(Duration::from_secs(10), out.settle(pending::<()>()))
            .await
            .expect("settling waited on the stuck stdout");
        assert_eq!(written(&log), ["E:note\n"]);
    }

    /// A Ctrl-C ends the wait on a stdout that has stopped, and one is enough:
    /// settling again, as the end of a session does, does not wait on it.
    #[tokio::test]
    async fn an_interrupt_ends_the_wait_for_a_stuck_stdout_once() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m0".into(), false);
        stdout.reached(1);
        out.room().await.expect("room").send("m1".into(), false);
        out.settle(std::future::ready(())).await;
        tokio::time::timeout(Duration::from_secs(10), out.settle(pending::<()>()))
            .await
            .expect("settling again waited on the stuck stdout");
    }

    /// Once the user has asked to go, what stdout had queued is not written,
    /// and it does not hold stderr up.
    #[tokio::test]
    async fn abandoning_drops_stdouts_backlog() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m0".into(), false);
        stdout.reached(1);
        out.room().await.expect("room").send("m1".into(), false);
        out.abandon();
        out.line("bye");
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["E:bye\n"]);

        stdout.let_through();
        tokio::time::timeout(Duration::from_secs(10), out.stdout.closed())
            .await
            .expect("the thread stopped at the next message");
        assert_eq!(written(&log), ["E:bye\n", "O:m0\n"]);
    }

    /// With one destination for both streams, a write waits for the other's:
    /// a line too long for a pipe to take at once is not cut by the other
    /// stream's.
    #[tokio::test]
    async fn one_destination_is_written_one_stream_at_a_time() {
        let log = Log::default();
        let (stdout, stderr) = (Gate::shut("O", &log), Gate::open("E", &log));
        let (out, _failures) = start_shared(&stdout, &stderr);

        out.room().await.expect("room").send("m".into(), false);
        stdout.reached(1);
        out.line("note");
        assert!(
            !stderr.entered(1, Duration::from_millis(100)),
            "stderr wrote into the middle of stdout's write"
        );

        stdout.let_through();
        out.drain_stdout().await;
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["O:m\n", "E:note\n"]);
    }

    /// With one destination, stderr's writes wait behind stdout's stuck one, so
    /// once stdout is abandoned settling waits on neither (#579). What is
    /// queued after, as the session closes, is written in turn once the write
    /// ends, and never inside it.
    #[tokio::test]
    async fn one_destination_abandoned_is_settled_without_waiting_on_either() {
        let log = Log::default();
        let (stdout, stderr) = (Gate::shut("O", &log), Gate::open("E", &log));
        let (out, _failures) = start_shared(&stdout, &stderr);

        out.room().await.expect("room").send("m".into(), false);
        stdout.reached(1);
        out.line("note");
        out.abandon();
        tokio::time::timeout(Duration::from_secs(10), out.settle(pending::<()>()))
            .await
            .expect("settling waited on stderr behind the stuck stdout");
        out.line("closing");
        out.line("report");

        // Written behind stdout's write, whole, and in the order queued.
        stdout.let_through();
        out.finish(pending::<()>()).await;
        assert_eq!(
            written(&log),
            ["O:m\n", "E:note\n", "E:closing\n", "E:report\n"]
        );
    }

    /// The last wait ends at the caller's bound even when stdout's stuck write
    /// is what holds stderr's up, so a destination that has stopped delays what
    /// is printed and not the exit.
    #[tokio::test]
    async fn finishing_behind_a_stuck_stdout_ends_at_the_callers_bound() {
        let log = Log::default();
        let (stdout, stderr) = (Gate::shut("O", &log), Gate::open("E", &log));
        let (out, _failures) = start_shared(&stdout, &stderr);

        out.room().await.expect("room").send("m".into(), false);
        stdout.reached(1);
        out.abandon();
        out.line("report");
        let bound = tokio::time::sleep(Duration::from_millis(100));
        tokio::time::timeout(Duration::from_secs(10), out.finish(bound))
            .await
            .expect("finishing waited past its bound");
        assert!(written(&log).is_empty(), "{:?}", written(&log));

        stdout.let_through();
        tokio::time::timeout(Duration::from_secs(10), out.stderr.closed())
            .await
            .expect("the stderr thread ended");
    }

    /// A framed message whose write was abandoned midway does not draw the
    /// prompt again once it finishes: the session is closing, and says so.
    #[tokio::test]
    async fn an_abandoned_framed_write_does_not_redraw_the_prompt() {
        let log = Log::default();
        let stdout = Gate::shut("O", &log);
        let (out, _failures) = start(&stdout, &Gate::open("E", &log));

        out.room().await.expect("room").send("m0".into(), true);
        stdout.reached(1);
        out.room().await.expect("room").send("m1".into(), false);
        out.abandon();
        stdout.let_through();
        // Stopped at m1, after it finished m0.
        tokio::time::timeout(Duration::from_secs(10), out.stdout.closed())
            .await
            .expect("the thread stopped at the next message");
        out.drain_stderr(false).await;
        assert_eq!(written(&log), ["E:\n", "O:m0\n"]);
    }

    /// A Ctrl-C that cuts the last wait short stops stderr's thread after the
    /// write it is in: what is still queued is dropped, not written beside
    /// what the caller writes next.
    #[tokio::test]
    async fn an_interrupted_finish_stops_stderr_after_its_current_write() {
        let log = Log::default();
        let stderr = Gate::shut("E", &log);
        let (out, _failures) = start(&Gate::open("O", &log), &stderr);

        out.line("a");
        stderr.reached(1);
        out.line("b");
        out.line("c");
        out.finish(std::future::ready(())).await;
        stderr.let_through();
        tokio::time::timeout(Duration::from_secs(10), out.stderr.closed())
            .await
            .expect("the stderr thread ended at the next item");
        assert_eq!(written(&log), ["E:a\n"]);
    }

    /// Finishing writes what was queued, and nothing queued after it: the
    /// caller writes stderr itself from then on.
    #[tokio::test]
    async fn nothing_is_written_to_stderr_after_it_is_finished() {
        let log = Log::default();
        let (out, _failures) = start(&Gate::open("O", &log), &Gate::open("E", &log));

        out.line("before");
        out.finish(pending::<()>()).await;
        tokio::time::timeout(Duration::from_secs(10), out.stderr.closed())
            .await
            .expect("the stderr thread ended");
        out.line("after");
        assert_eq!(written(&log), ["E:before\n"]);
    }
}
