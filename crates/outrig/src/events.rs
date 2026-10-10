//! The session's event stream: what its agents did, and what was done to
//! them, numbered as it happens and handed to every subscriber.
//!
//! The catalog is public, in [`crate::harness::event`]; this is the part that
//! publishes it. [`Events::emit`] numbers an event and hands it to each
//! subscriber's queue under one lock, so every subscriber sees the same order,
//! and it never waits: a queue that is full drops its oldest event and counts
//! it, for its reader to be told. Nothing a subscriber does -- a stalled disk
//! under the event log, an embedder that stops reading -- can hold up a round,
//! an execution, or a shutdown.
//!
//! `<log_dir>/events.jsonl` is written by one such subscriber
//! ([`StreamBuilder::record`]). The envelope is CloudEvents 1.0 and the
//! payloads are OutRig's. The top level carries the standard context
//! attributes and nothing else, because CloudEvents attribute names are
//! lower-case letters and digits only: everything OutRig-specific is inside
//! `data`. `doc/reference/events.md` is the schema.
//!
//! # The log's backpressure is its own
//!
//! The writer takes an event from its subscription only once the file has
//! room for it, so a disk that falls behind leaves events waiting in that
//! subscription, whose overflow is a counted gap -- never in the session. A
//! gap shows in the file as a jump in `id`, and [`Events::close`] reports it
//! with everything else the file did not get.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::{IoPathExt, OutrigError, Result};
use crate::harness::LogLoss;
use crate::harness::event::{Event, Payload, Received, Subject, Subscription, TryRecvError};
use crate::line_sink::{self, Labels, LineSink, Loss};

/// The log's name under the session's log directory.
pub(crate) const EVENTS_LOG: &str = "events.jsonl";

/// Who can read it: its owner. It holds message bodies the model may never
/// have printed, which is a different sensitivity from a connection log.
const MODE: u32 = 0o600;

/// The type prefix: the reverse-DNS name OutRig's labels already use.
const TYPE_PREFIX: &str = "org.outrig.";

/// The protocol's id for the one agent a session runs: the `subject` of an
/// event that belongs to it, and the end a message to or from it names.
pub(crate) const PRIMARY_SUBJECT: &str = "agent/primary";

static EVENT_LABELS: Labels = Labels {
    what: "agent event",
    claimant: "agent",
    warn: |args| tracing::warn!(target: "outrig::events", "{args}"),
    error: |args| tracing::error!(target: "outrig::events", "{args}"),
};

/// Where a session's events go. Cheap to clone; every clone publishes into
/// the one stream. [`Events::off`] publishes nowhere, and is what a session
/// with no subscriber holds.
#[derive(Clone, Default)]
pub(crate) struct Events {
    stream: Option<Arc<Stream>>,
}

struct Stream {
    /// What numbering and handing out share: the one lock `emit` takes.
    published: Mutex<Published>,
    /// The embedder's subscriptions, in the order they were made, then the
    /// log's.
    queues: Box<[Arc<Queue>]>,
    /// How many of `queues` are the embedder's.
    subscribers: usize,
    /// The log's writer, until a close takes it.
    file: Mutex<Option<FileWriter>>,
}

struct Published {
    /// The id the last event was given. Ids start at 1.
    last: u64,
    /// Set once the stream has ended: nothing more is numbered.
    closed: bool,
}

/// How a session's stream was delivered, as far as it had got.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    /// The id of the last event published.
    pub(crate) last: u64,
    /// How many events each of the embedder's subscriptions lost, in the order
    /// they were made.
    pub(crate) missed: Vec<u64>,
}

impl Events {
    /// No subscriber: every emit is dropped.
    pub(crate) fn off() -> Self {
        Self::default()
    }

    /// Whether events go anywhere. Fixed when the stream is built, so a caller
    /// can skip building an event nobody will receive.
    pub(crate) fn is_on(&self) -> bool {
        self.stream.is_some()
    }

    /// Publish `payload`, now. Never waits: a subscriber with no room loses its
    /// oldest event instead, and one emitted after the stream ended is
    /// dropped, there being nobody left to hand it to.
    pub(crate) fn emit(&self, payload: Payload) {
        if let Some(stream) = &self.stream {
            stream.publish(payload, false);
        }
    }

    /// [`Events::emit`] the payload `build` makes, building it only when
    /// someone subscribes: for one that copies what it carries.
    pub(crate) fn emit_with(&self, build: impl FnOnce() -> Payload) {
        if let Some(stream) = &self.stream {
            stream.publish(build(), false);
        }
    }

    /// Publish `last` as the stream's final event, and end it there, in one
    /// step: an event emitted concurrently lands before it or not at all.
    /// Then finish the log -- see [`Events::close`].
    pub(crate) async fn close_after(&self, last: Payload) -> std::result::Result<(), LogLoss> {
        match &self.stream {
            Some(stream) => {
                stream.publish(last, true);
                stream.finish().await
            }
            None => Ok(()),
        }
    }

    /// End the stream, and wait until the log holds every event it was given
    /// -- at most [`line_sink::SHUTDOWN_GRACE`]. What it does not hold by then
    /// is reported, counted, rather than waited for. A subscription keeps what
    /// it holds, and reads to the end of it. A session ends its stream with
    /// [`Events::close_after`] instead.
    #[cfg(test)]
    pub(crate) async fn close(&self) -> std::result::Result<(), LogLoss> {
        match &self.stream {
            Some(stream) => {
                stream.end();
                stream.finish().await
            }
            None => Ok(()),
        }
    }

    /// How far the stream has got, and what each of the embedder's
    /// subscriptions has lost. Final once the stream has ended.
    pub(crate) fn tally(&self) -> Tally {
        let Some(stream) = &self.stream else {
            return Tally::default();
        };
        Tally {
            last: lock(&stream.published).last,
            missed: stream.queues[..stream.subscribers]
                .iter()
                .map(|queue| queue.missed())
                .collect(),
        }
    }
}

impl Stream {
    /// Number `payload` and hand it to every queue; with `last`, end the stream
    /// behind it.
    fn publish(&self, payload: Payload, last: bool) {
        // Built before the lock every emitter shares, and given its place
        // under it.
        let subject = payload.subject();
        let mut event = Arc::new(Event {
            id: 0,
            time: UNIX_EPOCH,
            subject,
            payload,
        });
        let mut evicted = Vec::new();
        {
            let mut published = lock(&self.published);
            if published.closed {
                tracing::debug!(
                    target: "outrig::events",
                    "not published, the stream having ended: {}",
                    event.kind()
                );
                return;
            }
            published.last += 1;
            let fresh = Arc::get_mut(&mut event).expect("nothing else holds it yet");
            fresh.id = published.last;
            fresh.time = SystemTime::now();
            for queue in &self.queues {
                evicted.extend(queue.push(&event));
            }
            if last {
                self.end_locked(&mut published);
            }
        }
        // Freed outside the lock: an evicted turn can be large.
        drop(evicted);
    }

    /// End the stream: nothing more is numbered, and every queue is closed.
    #[cfg(test)]
    fn end(&self) {
        self.end_locked(&mut lock(&self.published));
    }

    fn end_locked(&self, published: &mut Published) {
        published.closed = true;
        self.queues.iter().for_each(|queue| queue.close());
    }

    /// Finish the log, once; a second call has nothing to report.
    async fn finish(&self) -> std::result::Result<(), LogLoss> {
        let file = lock(&self.file).take();
        match file {
            Some(file) => file.close().await,
            None => Ok(()),
        }
    }
}

/// A stream dropped without a close still ends every subscription, and lets
/// the log's writer finish in the background.
impl Drop for Stream {
    fn drop(&mut self) {
        self.queues.iter().for_each(|queue| queue.close());
    }
}

/// One subscriber's queue: the events it has not taken, oldest first, and
/// what it lost.
pub(crate) struct Queue {
    capacity: usize,
    buffer: Mutex<Buffer>,
    /// Woken by each event pushed, and by the close.
    ready: Notify,
}

#[derive(Default)]
struct Buffer {
    events: VecDeque<Arc<Event>>,
    /// Dropped since the reader last took anything, and not yet told.
    unreported: u64,
    /// Dropped in all.
    missed: u64,
    closed: bool,
}

impl Queue {
    fn new(capacity: usize) -> Arc<Self> {
        assert!(capacity > 0, "a subscription holds at least one event");
        Arc::new(Self {
            capacity,
            buffer: Mutex::new(Buffer::default()),
            ready: Notify::new(),
        })
    }

    /// Hold `event` for the reader, dropping the oldest held if there is no
    /// room; that one is handed back, for the caller to free. Never waits.
    fn push(&self, event: &Arc<Event>) -> Option<Arc<Event>> {
        let mut buffer = lock(&self.buffer);
        if buffer.closed {
            return None;
        }
        let evicted = if buffer.events.len() == self.capacity {
            buffer.unreported += 1;
            buffer.missed += 1;
            buffer.events.pop_front()
        } else {
            None
        };
        buffer.events.push_back(Arc::clone(event));
        drop(buffer);
        // One reader, so a permit stored for it is never lost.
        self.ready.notify_one();
        evicted
    }

    fn close(&self) {
        lock(&self.buffer).closed = true;
        self.ready.notify_one();
    }

    /// What the reader is owed next: what it lost, before the event after
    /// it; then the oldest event held.
    pub(crate) fn next(&self) -> std::result::Result<Received, TryRecvError> {
        let mut buffer = lock(&self.buffer);
        if buffer.unreported > 0 {
            return Ok(Received::Missed(std::mem::take(&mut buffer.unreported)));
        }
        match buffer.events.pop_front() {
            Some(event) => Ok(Received::Event(event)),
            None if buffer.closed => Err(TryRecvError::Closed),
            None => Err(TryRecvError::Empty),
        }
    }

    /// [`Queue::next`], waiting for it; `None` once closed and empty. Only
    /// `next` takes anything, so dropping this takes nothing.
    pub(crate) async fn recv(&self) -> Option<Received> {
        loop {
            match self.next() {
                Ok(received) => return Some(received),
                Err(TryRecvError::Closed) => return None,
                Err(TryRecvError::Empty) => self.ready.notified().await,
            }
        }
    }

    pub(crate) fn missed(&self) -> u64 {
        lock(&self.buffer).missed
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Give up on what is held: how many were dropped and not yet told, and
    /// how many were held and not taken.
    fn abandon(&self) -> (u64, u64) {
        let mut buffer = lock(&self.buffer);
        buffer.closed = true;
        let unread = buffer.events.len() as u64;
        buffer.events.clear();
        (std::mem::take(&mut buffer.unreported), unread)
    }
}

/// What a session's stream will be: its subscriptions and its log, made before
/// the first event so each sees every one.
#[derive(Default)]
pub(crate) struct StreamBuilder {
    queues: Vec<Arc<Queue>>,
    file: Option<FileWriter>,
}

impl StreamBuilder {
    /// A subscription holding at most `capacity` events for its reader.
    pub(crate) fn subscribe(&mut self, capacity: usize) -> Subscription {
        let queue = Queue::new(capacity);
        self.queues.push(Arc::clone(&queue));
        Subscription::new(queue)
    }

    /// Record the stream in `<log_dir>/events.jsonl` for the session `source`
    /// names.
    ///
    /// A log that already holds a recording is refused, and left as it is.
    /// Its events carry this `source` with ids from 1, as a second recording's
    /// would -- a session started again in the same directory -- and a reader
    /// that takes `source` and `id` as an event's identity, as CloudEvents
    /// says to, would drop the second's as repeats.
    pub(crate) async fn record(&mut self, log_dir: &Path, source: String) -> Result<()> {
        self.file = Some(FileWriter::open(log_dir, source).await?);
        Ok(())
    }

    /// Record into `sink`, which writes `path`.
    #[cfg(test)]
    pub(crate) fn record_over(&mut self, sink: LineSink<()>, path: PathBuf, source: String) {
        self.file = Some(FileWriter::over(sink, path, source));
    }

    /// The stream, or [`Events::off`] when nothing subscribed.
    pub(crate) fn build(mut self) -> Events {
        let mut queues = std::mem::take(&mut self.queues);
        let subscribers = queues.len();
        let file = self.file.take();
        if let Some(file) = &file {
            queues.push(Arc::clone(&file.queue));
        }
        if queues.is_empty() {
            return Events::off();
        }
        Events {
            stream: Some(Arc::new(Stream {
                published: Mutex::new(Published {
                    last: 0,
                    closed: false,
                }),
                queues: queues.into_boxed_slice(),
                subscribers,
                file: Mutex::new(file),
            })),
        }
    }
}

/// A builder dropped unbuilt -- a session that failed to start -- ends its
/// subscriptions, and lets go of its log.
impl Drop for StreamBuilder {
    fn drop(&mut self) {
        self.queues.iter().for_each(|queue| queue.close());
        if let Some(file) = &self.file {
            file.queue.close();
        }
    }
}

/// `events.jsonl`'s writer: a subscription of its own, and the task that
/// takes from it into the file.
struct FileWriter {
    path: PathBuf,
    queue: Arc<Queue>,
    sink: LineSink<()>,
    task: JoinHandle<()>,
}

impl FileWriter {
    async fn open(log_dir: &Path, source: String) -> Result<Self> {
        let path = log_dir.join(EVENTS_LOG);
        let sink = LineSink::open(&path, Some(MODE), line_sink::QUEUE, &EVENT_LABELS).await?;
        // Read under the claim the sink holds, so nothing appends in between.
        // Dropping the sink on the error lets its writer end and the claim go.
        let held = tokio::fs::metadata(&path)
            .await
            .path_ctx("stat", &path)?
            .len();
        if held > 0 {
            return Err(OutrigError::Configuration(format!(
                "the agent event log {} already holds a recording ({held} bytes); a recording \
                 starts a log of its own, so each event's source and id name it alone. Move it \
                 aside, or record in a fresh session",
                path.display()
            )));
        }
        Ok(Self::over(sink, path, source))
    }

    /// Write what a fresh subscription receives into `sink`, which writes
    /// `path`.
    fn over(sink: LineSink<()>, path: PathBuf, source: String) -> Self {
        let queue = Queue::new(crate::harness::event::DEFAULT_CAPACITY);
        let task = tokio::spawn(write_events(
            Subscription::new(Arc::clone(&queue)),
            sink.clone(),
            source,
        ));
        Self {
            path,
            queue,
            sink,
            task,
        }
    }

    /// Wait for the writer to take what its subscription holds and the file
    /// to hold it, all within one [`line_sink::SHUTDOWN_GRACE`], and report
    /// everything the file did not get: what the subscription lost, what was
    /// left unread, what failed to write, and what the sink still held.
    async fn close(mut self) -> std::result::Result<(), LogLoss> {
        let deadline = tokio::time::Instant::now() + line_sink::SHUTDOWN_GRACE;
        self.queue.close();
        if tokio::time::timeout_at(deadline, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
        let (unreported, unread) = self.queue.abandon();
        self.sink.lose_many(&(), unreported, behind());
        self.sink.lose_many(
            &(),
            unread,
            std::io::Error::other(format!(
                "the agent event writer did not take them within {:?}, so they were never \
                 written",
                line_sink::SHUTDOWN_GRACE
            )),
        );
        // A writer that had to be stopped holding nothing lost nothing, so
        // only what the sink counted is reported.
        let _ = self.sink.close_by(deadline).await;
        match self.sink.take_loss(&()) {
            None => Ok(()),
            Some(Loss {
                records,
                source,
                integrity,
            }) => Err(LogLoss {
                path: self.path,
                records,
                first: source.to_string(),
                integrity: integrity.map(|why| why.to_string()),
            }),
        }
    }
}

/// Why events the writer's subscription dropped were lost.
fn behind() -> std::io::Error {
    std::io::Error::other(format!(
        "the agent event writer fell more than {} events behind the session",
        crate::harness::event::DEFAULT_CAPACITY
    ))
}

/// Take each event from `events` into `sink` as one line, once the file has
/// room for it -- the writer's own backpressure, which stops at its
/// subscription.
async fn write_events(mut events: Subscription, sink: LineSink<()>, source: String) {
    let room = sink.room();
    loop {
        room.below(line_sink::QUEUE as u64).await;
        // No await between taking an event and queueing it, so a writer
        // stopped at either await loses nothing it does not count.
        match events.recv().await {
            None => return,
            Some(Received::Missed(lost)) => sink.lose_many(&(), lost, behind()),
            Some(Received::Event(event)) => match envelope(&event, &source) {
                Ok(line) => {
                    sink.try_enqueue((), line, || {
                        format!(
                            "more than {} events were waiting for the agent event writer",
                            line_sink::QUEUE
                        )
                    });
                }
                Err(e) => {
                    let why = format!("encoding {} failed: {e}", event.kind());
                    tracing::warn!(target: "outrig::events", "{why}");
                    sink.lose(&(), std::io::Error::other(why));
                }
            },
        }
    }
}

/// One record: the CloudEvents context attributes, then `data`, on a line.
fn envelope(event: &Event, source: &str) -> serde_json::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct Record<'a> {
        specversion: &'static str,
        id: String,
        source: &'a str,
        #[serde(rename = "type")]
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<&'a str>,
        time: String,
        datacontenttype: &'static str,
        data: &'a Payload,
    }
    let time = jiff::Timestamp::try_from(event.time).unwrap_or_else(|_| jiff::Timestamp::now());
    let mut line = serde_json::to_vec(&Record {
        specversion: "1.0",
        id: event.id.to_string(),
        source,
        kind: format!("{TYPE_PREFIX}{}", event.kind()),
        subject: event.subject.as_ref().map(Subject::as_str),
        time: time.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        datacontenttype: "application/json",
        data: &event.payload,
    })?;
    line.push(b'\n');
    Ok(line)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) use self::testing::*;

/// Opening a log, and reading one back, for tests across the crate.
#[cfg(test)]
mod testing {
    use std::path::Path;

    use serde_json::Value;

    use super::Events;

    /// The `source` of a test's log.
    pub(crate) const TEST_SOURCE: &str = "/outrig/session/test";

    /// A stream recording to a log in `dir`, as a session opens one.
    pub(crate) async fn opened(dir: &Path) -> Events {
        let mut stream = super::StreamBuilder::default();
        stream
            .record(dir, TEST_SOURCE.to_string())
            .await
            .expect("open the log");
        stream.build()
    }

    /// Wait until nothing holds the lock on `log_dir`'s closed log, for a test
    /// that opens it again.
    ///
    /// Closing the log does not release its lock at once when another test
    /// is starting a process. `flock` belongs to the open file description,
    /// and a process being spawned holds a copy of the descriptor table it
    /// was cloned with until it execs, so the lock outlives the writer by
    /// that long: a fraction of a millisecond, often enough for a reopen to
    /// be refused as "already owned" rather than for what it holds.
    pub(crate) async fn released(log_dir: &Path) {
        let path = log_dir.join(super::EVENTS_LOG);
        tokio::task::spawn_blocking(move || {
            let log = std::fs::File::open(&path).expect("open the log");
            nix::fcntl::Flock::lock(log, nix::fcntl::FlockArg::LockExclusive)
                .map_err(|(_, errno)| errno)
                .expect("lock the log");
        })
        .await
        .expect("wait for the log's lock");
    }

    /// Every record in `log_dir`'s `events.jsonl`, in order, each checked to
    /// parse.
    pub(crate) fn recorded(log_dir: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(log_dir.join(super::EVENTS_LOG)).expect("read events");
        text.lines()
            .map(|line| serde_json::from_str(line).expect("every line is a whole record"))
            .collect()
    }

    /// The type of each record, without the prefix.
    pub(crate) fn kinds(records: &[Value]) -> Vec<String> {
        records
            .iter()
            .map(|record| {
                record["type"]
                    .as_str()
                    .and_then(|kind| kind.strip_prefix(super::TYPE_PREFIX))
                    .expect("an OutRig type")
                    .to_string()
            })
            .collect()
    }

    /// The `data` of each record of type `kind`, in order.
    pub(crate) fn of_kind<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
        let kind = format!("{}{kind}", super::TYPE_PREFIX);
        records
            .iter()
            .filter(|record| record["type"] == kind.as_str())
            .map(|record| &record["data"])
            .collect()
    }
}
