//! An append-only file of JSON lines, written by the one task that owns it.
//!
//! The session's logs are each a sequence of whole records: `network.jsonl`
//! one per connection, `events.jsonl` one per thing an agent did. What keeps
//! them that way is the same for both, and lives here: a bounded queue that
//! applies backpressure rather than dropping, a reserve-then-send enqueue so a
//! cancelled producer leaves no phantom count, an exclusive lock on the file,
//! rollback to the last whole line on a partial write, poisoning when that
//! rollback cannot be proven, and bounded accounting of whatever was lost.
//!
//! What a record says and whose it is belong to the caller. A record is handed
//! over already encoded, under an owner `K` the caller chooses -- the network
//! interceptor's attachment, or nobody in particular -- and a loss comes back
//! filed under that owner, for the caller to report in its own terms.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{IoPathExt, OutrigError, Result};

/// How many records may be queued ahead of the writer, unless the caller says
/// otherwise. Bounded so a stalled log applies backpressure to whatever
/// produces records instead of growing, and so nothing can make a session hold
/// an unbounded number of them by producing them faster than the disk accepts
/// them.
pub(crate) const QUEUE: usize = 1024;

/// How long a drain or a close waits for the writer before giving up on it.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// What a log calls itself in what it reports, and where it reports it.
///
/// Every message a sink produces is built from these, so each caller's read
/// as they always have. The two functions carry the caller's tracing target,
/// which has to be a constant at the call site.
#[derive(Debug)]
pub(crate) struct Labels {
    /// What a record is: `"network audit"` gives "the network audit log" and
    /// "the network audit writer".
    pub(crate) what: &'static str,
    /// What owns the file, named when a second one is refused it.
    pub(crate) claimant: &'static str,
    pub(crate) warn: fn(fmt::Arguments<'_>),
    pub(crate) error: fn(fmt::Arguments<'_>),
}

/// Per owner: how many of its records were lost, and the failure worth
/// telling someone about.
pub(crate) type Losses<K> = Arc<Mutex<BTreeMap<K, Loss>>>;

/// Per owner: how many of its records the writer has taken and not yet
/// answered for. Zero entries are removed rather than kept.
pub(crate) type Pending<K> = Arc<Mutex<BTreeMap<K, u64>>>;

/// One owner's losses: how many records, the failure that broke the first
/// append, and -- if the log may hold a partial record -- what stopped the
/// writer proving otherwise.
#[derive(Debug)]
pub(crate) struct Loss {
    pub(crate) records: u64,
    pub(crate) source: io::Error,
    pub(crate) integrity: Option<io::Error>,
}

/// What the writer is asked to do.
pub(crate) enum Job<K> {
    /// One record, already encoded, with the owner it belongs to and a channel
    /// the writer answers on once it has dealt with it. A producer that waits
    /// on that knows its record is in the file when the wait ends -- the queue
    /// moves the bytes out of reach of the producer's cancellation without
    /// moving the guarantee.
    Record {
        who: K,
        line: Vec<u8>,
        done: oneshot::Sender<()>,
    },
    /// Answer once everything queued ahead of this has been written. This is
    /// what lets a caller say "every record I owed is on disk" rather than
    /// assume it.
    Drained(oneshot::Sender<()>),
}

/// A handle on one log's writer. Cheap to clone; every clone queues to the one
/// writer, which ends once the last clone's sender is gone.
#[derive(Debug)]
pub(crate) struct LineSink<K> {
    /// Records queued for the writer. Bounded, so a sink that cannot keep up
    /// applies backpressure to whatever produces records rather than growing
    /// without limit. `None` once [`close`](Self::close) has ended the writer.
    pub(crate) records: Option<mpsc::Sender<Job<K>>>,
    /// The writer's handle, so [`close`](Self::close) can end it and wait.
    /// Shared by every clone, and taken by whichever closes first.
    pub(crate) writer: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// Records the writer has been handed and has not answered for, per owner.
    ///
    /// The writer is the only thing that knows what is in its queue, and
    /// stopping it by abort takes that with it -- so the same fact is kept
    /// where a caller can still read it. Whatever is left here when the writer
    /// stops is exactly what it accepted and never accounted for, by owner and
    /// by count.
    pub(crate) pending: Pending<K>,
    /// What the writer could not write, per owner: the first failure and how
    /// many followed it.
    ///
    /// Bounded on purpose -- one entry per owner rather than per record --
    /// because whatever produces records can make writing fail as often as it
    /// likes, and an outage such as `ENOSPC` would otherwise grow host memory,
    /// and the error reporting it, for as long as it lasted.
    pub(crate) unwritten: Losses<K>,
    /// Woken each time the writer has dealt with a record, and when a close
    /// has accounted for what it held.
    freed: Arc<Notify>,
    labels: &'static Labels,
}

// By hand: a derived `Clone` would require `K: Clone` of a handle that holds
// no `K` of its own.
impl<K> Clone for LineSink<K> {
    fn clone(&self) -> Self {
        Self {
            records: self.records.clone(),
            writer: Arc::clone(&self.writer),
            pending: Arc::clone(&self.pending),
            unwritten: Arc::clone(&self.unwritten),
            freed: Arc::clone(&self.freed),
            labels: self.labels,
        }
    }
}

/// How full a sink is, readable without holding one of its senders -- which
/// would keep its writer from finishing.
pub(crate) struct Room<K> {
    pending: Pending<K>,
    freed: Arc<Notify>,
}

impl<K> Room<K> {
    /// Wait until the writer holds fewer than `held` records it has not dealt
    /// with: those queued and the one it is writing.
    pub(crate) async fn below(&self, held: u64) {
        loop {
            // Enabled before the count is read, so a record dealt with
            // between the two still wakes it.
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let count: u64 = self
                .pending
                .lock()
                .map_or(0, |pending| pending.values().sum());
            if count < held {
                return;
            }
            freed.await;
        }
    }
}

impl<K: Ord + Clone + Send + 'static> LineSink<K> {
    /// Open `path` for appending, claim it, and start its writer.
    ///
    /// `perm`, when given, is the file's mode: set as it is created and again
    /// on the open file, so neither the umask nor an earlier file's mode
    /// decides who can read it. `queue` is how many records may wait for the
    /// writer.
    pub(crate) async fn open(
        path: &Path,
        perm: Option<u32>,
        queue: usize,
        labels: &'static Labels,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .path_ctx("create directory", parent)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        if let Some(perm) = perm {
            options.mode(perm);
        }
        let file = options.open(path).await.path_ctx("open", path)?;
        // Claimed exclusively, because the writer's rollback truncates this
        // file back to where a failed record began -- and `ftruncate` acts on
        // the inode, not on one descriptor's appends. A second writer's record
        // sitting past that mark would be destroyed, and its owner would
        // report a clean teardown having lost it. The lock is advisory and
        // held by the open file description, so it also refuses a second
        // writer in another process, and the kernel drops it when the file
        // closes with the writer.
        //
        // `flock` on a `tokio::fs::File` is a raw-fd call and does not block:
        // the non-blocking form is the point, since the answer wanted here is
        // "someone already has this" rather than a wait.
        //
        // Only for a regular file, which is the only thing that truncation
        // means anything for. A caller who points this at a device or a pipe
        // has no rollback to protect and no exclusivity to lose -- and the
        // tests that need every write to fail point it at `/dev/full`, whose
        // one inode every one of them would otherwise queue behind.
        //
        // The exemption is for a file positively known not to be regular. A
        // `stat` that *failed* says nothing, and taking the exemption on it
        // would pair a truncating writer with no lock, which is the one
        // combination this exists to prevent.
        if claims_exclusively(file.metadata().await).path_ctx("stat", path)? {
            // The typed `fcntl::Flock` that replaces this takes *ownership*
            // of the file and unlocks on drop, and the writer owns a
            // `tokio::fs::File` it goes on appending to -- so the lock has to
            // outlive the call that takes it, which is what the raw form does.
            #[allow(deprecated)]
            nix::fcntl::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                nix::fcntl::FlockArg::LockExclusiveNonblock,
            )
            .map_err(|e| {
                OutrigError::Configuration(format!(
                    "the {} log {} is already owned by another {} ({e}); one writer owns \
                     it, because the rollback that keeps it a sequence of whole records \
                     truncates the file",
                    labels.what,
                    path.display(),
                    labels.claimant,
                ))
            })?;
            // Again on the open file, under the lock: `mode` applies only to a
            // file this open created, and the umask narrows even that.
            if let Some(perm) = perm {
                file.set_permissions(std::fs::Permissions::from_mode(perm))
                    .await
                    .path_ctx("set permissions on", path)?;
            }
        }

        let (records, queued) = mpsc::channel(queue);
        let mut sink = Self::over(records, None, labels);
        // The writer owns the file and is the only thing that touches it, so
        // no caller's cancellation can land inside a record. It ends when the
        // last sink handle drops.
        let writer = tokio::spawn(write_lines(file, queued, sink.clone_parts()));
        sink.writer = Arc::new(Mutex::new(Some(writer)));
        Ok(sink)
    }

    /// A sink queuing to `records`, whose writer is `writer`, holding nothing
    /// yet.
    fn over(
        records: mpsc::Sender<Job<K>>,
        writer: Option<JoinHandle<()>>,
        labels: &'static Labels,
    ) -> Self {
        Self {
            records: Some(records),
            writer: Arc::new(Mutex::new(writer)),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            unwritten: Arc::new(Mutex::new(BTreeMap::new())),
            freed: Arc::new(Notify::new()),
            labels,
        }
    }

    /// A sink queuing to `records`, whose other end a test holds, with `writer`
    /// as its writer: one that never ends, say, or none at all.
    #[cfg(test)]
    pub(crate) fn queuing_to(
        records: mpsc::Sender<Job<K>>,
        writer: Option<JoinHandle<()>>,
        labels: &'static Labels,
    ) -> Self {
        Self::over(records, writer, labels)
    }

    /// What the writer shares with every handle, without a sender.
    fn clone_parts(&self) -> Parts<K> {
        Parts {
            pending: Arc::clone(&self.pending),
            unwritten: Arc::clone(&self.unwritten),
            freed: Arc::clone(&self.freed),
            labels: self.labels,
        }
    }

    /// How full the sink is, for a caller that waits for room without holding
    /// a sender.
    pub(crate) fn room(&self) -> Room<K> {
        Room {
            pending: Arc::clone(&self.pending),
            freed: Arc::clone(&self.freed),
        }
    }

    /// Waits until every record queued so far has been written.
    ///
    /// Bounded: a writer that cannot drain is reported rather than waited on
    /// forever, since the caller has a teardown to finish.
    pub(crate) async fn drain(&self) -> Result<()> {
        // One deadline over both halves. Getting the marker *into* the queue
        // is itself a wait -- the queue is bounded, and a stalled writer with
        // every slot full never accepts it -- so timing only the answer would
        // leave the caller blocked here forever and never reach whatever it
        // has to undo behind this.
        let Some(records) = self.records.as_ref() else {
            return Ok(());
        };
        let queued = tokio::time::timeout(SHUTDOWN_GRACE, async {
            let (done, wait) = oneshot::channel();
            if records.send(Job::Drained(done)).await.is_err() {
                // The writer is gone; nothing is still queued behind it.
                return Ok(());
            }
            wait.await.map_err(|_| ())
        })
        .await;
        match queued {
            Ok(Ok(())) => Ok(()),
            _ => Err(OutrigError::Configuration(format!(
                "the {} log did not finish writing within {SHUTDOWN_GRACE:?}",
                self.labels.what
            ))),
        }
    }

    /// End the writer and wait for it to finish, so nothing can record a loss
    /// after the sweep that follows.
    ///
    /// Returns what went wrong if it could not be waited out; the sweep runs
    /// either way, since a writer that will not stop is a reason to report
    /// rather than a reason to skip collecting what it already recorded.
    pub(crate) async fn close(&mut self) -> Option<OutrigError> {
        let writer = self.writer.lock().ok().and_then(|mut w| w.take())?;
        // Every other handle is gone by now, or about to be. Taking the
        // sender ends the writer's loop once its queue is empty; a record
        // offered afterwards has nowhere to go and is reported rather than
        // lost quietly.
        self.records.take();
        // Held by reference, then aborted and joined -- not moved into the
        // timeout. Dropping a timed-out `JoinHandle` *detaches* the task, and
        // a detached writer goes on appending and recording losses after the
        // sweep that was supposed to be the last word on both.
        let mut writer = writer;
        let what = self.labels.what;
        let stopped = match tokio::time::timeout(SHUTDOWN_GRACE, &mut writer).await {
            Ok(Ok(())) => return None,
            // It ended on its own, badly. Whatever it was holding is as lost
            // as if it had been stopped, so the same accounting runs.
            Ok(Err(joined)) => format!("the {what} writer ended abnormally: {joined}"),
            Err(_) => {
                writer.abort();
                let _ = writer.await;
                format!(
                    "the {what} writer did not finish within {SHUTDOWN_GRACE:?} and was stopped"
                )
            }
        };
        // Aborting ends the task, not the syscall. `tokio::fs` runs its writes
        // on a blocking pool, and one already submitted completes whatever
        // happens to the future awaiting it -- so bytes may still land, and the
        // rollback that would have undone a partial write will not run because
        // the task that does it is gone. The log is therefore of unknown
        // integrity, and that is recorded where the sweep will find it rather
        // than only described in the error returned here.
        //
        // Per owner and by count, from what the writer was actually holding.
        self.convert_pending(&stopped);
        self.freed.notify_waiters();
        Some(OutrigError::Configuration(format!(
            "{stopped}; what it still held is unaccounted for"
        )))
    }

    /// File everything the writer accepted and never answered for as lost, by
    /// the owner that queued it.
    ///
    /// Called only once the writer is joined, so nothing can still be moving
    /// records out of `pending` while this reads it.
    fn convert_pending(&self, stopped: &str) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        for (who, records) in std::mem::take(&mut *pending) {
            Self::remember_stopped(&self.unwritten, &who, records, stopped);
        }
    }

    /// Takes the losses recorded for `who`.
    pub(crate) fn take_loss(&self, who: &K) -> Option<Loss> {
        self.unwritten.lock().ok()?.remove(who)
    }

    /// Every loss still on the books, whichever owner incurred it. Each is
    /// taken once.
    pub(crate) fn take_every_loss(&self) -> Vec<(K, Loss)> {
        let Ok(mut unwritten) = self.unwritten.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *unwritten).into_iter().collect()
    }

    /// Queue `line` for the writer as one of `who`'s records, waiting for room
    /// in the queue; the receiver answers once the writer has dealt with it.
    ///
    /// The bytes never leave the caller's hands half-sent, so nothing a caller
    /// does can cut a record in half: what gets cancelled here is the
    /// *queueing*, which leaves a record absent rather than a line truncated.
    pub(crate) async fn enqueue(&self, who: K, line: Vec<u8>) -> Result<oneshot::Receiver<()>> {
        let (done, written) = oneshot::channel();
        let records = self.records.as_ref().ok_or_else(|| self.stopped())?;
        // Capacity first, then the count, then the send -- and no await
        // between the last two, which is what makes the count mean something.
        //
        // The queue is bounded, so offering a record can wait, and a producer
        // cancelled in that wait used to leave its owner counted for a record
        // the writer never saw: over-reported if the writer was later stopped,
        // and silently dropped if it closed cleanly, which is a record missing
        // from the log with nothing saying so. `reserve` is cancellation-safe
        // -- tokio guarantees nothing was sent if it is dropped -- and the
        // permit it hands back sends synchronously and infallibly, so the
        // record is counted only once nothing can stop it being queued.
        let Ok(permit) = records.reserve().await else {
            return Err(self.stopped());
        };
        Self::enter_pending(&self.pending, &who);
        permit.send(Job::Record { who, line, done });
        Ok(written)
    }

    /// Queue `line` as one of `who`'s records if there is room now, without
    /// waiting. A record there is no room for is lost, and counted as `who`'s
    /// like any other loss, with `full` as why. Whether it was queued.
    pub(crate) fn try_enqueue(&self, who: K, line: Vec<u8>, full: impl FnOnce() -> String) -> bool {
        let Some(records) = self.records.as_ref() else {
            return false;
        };
        match records.try_reserve() {
            Ok(permit) => {
                let (done, _) = oneshot::channel();
                Self::enter_pending(&self.pending, &who);
                permit.send(Job::Record { who, line, done });
                true
            }
            Err(mpsc::error::TrySendError::Full(())) => {
                self.lose(&who, io::Error::other(full()));
                false
            }
            Err(mpsc::error::TrySendError::Closed(())) => false,
        }
    }

    /// Count one of `who`'s records lost before it reached the writer, for
    /// `why`.
    pub(crate) fn lose(&self, who: &K, why: io::Error) {
        Self::remember_unwritten(&self.unwritten, who, why);
    }

    /// [`enqueue`](Self::enqueue), then wait for the writer to have dealt with
    /// the record, so a caller that returns knows it is in the file.
    pub(crate) async fn write(&self, who: K, line: Vec<u8>) -> Result<()> {
        let written = self.enqueue(who, line).await?;
        // Deliberately *not* released on this error: the writer took the
        // record and then died holding it, so it is one of the records
        // `close` reports, under the owner that is still counted.
        written.await.map_err(|_| self.stopped())
    }

    fn stopped(&self) -> OutrigError {
        OutrigError::Configuration(format!("the {} writer has stopped", self.labels.what))
    }

    fn enter_pending(pending: &Mutex<BTreeMap<K, u64>>, who: &K) {
        if let Ok(mut pending) = pending.lock() {
            *pending.entry(who.clone()).or_insert(0) += 1;
        }
    }

    /// Releases one of `who`'s counted records. The writer is the only caller:
    /// a record is counted when nothing can stop it reaching the queue, so
    /// from there on the writer is the only thing that can account for it.
    fn leave_pending(pending: &Mutex<BTreeMap<K, u64>>, who: &K) {
        if let Ok(mut pending) = pending.lock()
            && let std::collections::btree_map::Entry::Occupied(mut held) =
                pending.entry(who.clone())
        {
            *held.get_mut() -= 1;
            if *held.get() == 0 {
                held.remove();
            }
        }
    }

    /// Records why the log cannot be trusted for `who`, without counting a
    /// record as lost on its own: a write that failed has already counted
    /// itself through [`remember_unwritten`](Self::remember_unwritten), and
    /// adding to the count here would report the same record twice.
    fn remember_integrity(unwritten: &Mutex<BTreeMap<K, Loss>>, who: &K, why: &str) {
        if let Ok(mut unwritten) = unwritten.lock() {
            let slot = unwritten.entry(who.clone()).or_insert_with(|| Loss {
                records: 1,
                source: io::Error::other(why.to_string()),
                integrity: None,
            });
            // Alongside the write failure, not instead of it: one says what
            // broke the append, the other what stopped it being undone, and a
            // reader needs both to know the file's state.
            slot.integrity = Some(io::Error::other(why.to_string()));
        }
    }

    /// Records `records` losses for `who`, with the file's integrity in doubt.
    ///
    /// The writer stopped holding them, so which one was mid-write is not
    /// knowable from here -- what is knowable is that the file may hold a
    /// partial line, and that is a fact about the file rather than about one
    /// record, so every owner that lost records is told it.
    fn remember_stopped(
        unwritten: &Mutex<BTreeMap<K, Loss>>,
        who: &K,
        records: u64,
        stopped: &str,
    ) {
        if let Ok(mut unwritten) = unwritten.lock() {
            let slot = unwritten.entry(who.clone()).or_insert_with(|| Loss {
                records: 0,
                source: io::Error::other(format!("{stopped}, so these records were never written")),
                integrity: None,
            });
            slot.records = slot.records.saturating_add(records);
            slot.integrity = Some(io::Error::other(format!(
                "{stopped} with a write in flight, so the log may hold a partial record that \
                 nothing rolled back"
            )));
        }
    }

    fn remember_unwritten(unwritten: &Mutex<BTreeMap<K, Loss>>, who: &K, error: io::Error) {
        if let Ok(mut unwritten) = unwritten.lock() {
            unwritten
                .entry(who.clone())
                .and_modify(|loss| loss.records = loss.records.saturating_add(1))
                .or_insert_with(|| Loss {
                    records: 1,
                    source: error,
                    integrity: None,
                });
        }
    }
}

/// Whether this file is one the writer has to claim exclusively.
///
/// Only a regular file: truncation is what the claim protects, and truncation
/// means nothing for a device or a pipe -- the tests that need every write to
/// fail point at `/dev/full`, whose single inode they would otherwise queue
/// on. A `stat` that *failed* is neither answer: taking the exemption on it
/// would pair a truncating writer with no lock, which is the one combination
/// the lock exists to prevent, so it is an error rather than a default.
pub(crate) fn claims_exclusively(opened: io::Result<std::fs::Metadata>) -> io::Result<bool> {
    opened.map(|f| f.is_file())
}

/// The one thing that writes a sink's file.
///
/// Owning the file in a single task is what makes a record atomic: no caller's
/// cancellation reaches the bytes, and there is exactly one writer, so no
/// interleaving either. It runs until the last sink handle drops.
///
/// "Written" here means handed to the filesystem and visible to anything that
/// reads the file. It is not `fsync`ed: the acknowledgement a producer waits
/// for says its record is in the file, not that it would survive the host
/// losing power. Durability would cost a sync per record, and nothing here
/// promises it.
async fn write_lines<K: Ord + Clone + Send + 'static>(
    mut file: tokio::fs::File,
    mut queue: mpsc::Receiver<Job<K>>,
    parts: Parts<K>,
) {
    let Parts {
        pending,
        unwritten,
        freed,
        labels,
    } = parts;
    let what = labels.what;
    // Set when the file may hold a partial record: see the rollback below.
    // Everything after that point is refused rather than appended -- but still
    // received, answered and counted, because a caller waiting for its record
    // is owed an answer and a record refused is still a record lost.
    let mut poisoned: Option<String> = None;
    while let Some(job) = queue.recv().await {
        let Job::Record { who, line, done } = job else {
            // A drain marker: everything queued ahead of it is written by the
            // time this is reached, so answering is the whole job. A receiver
            // that has given up is not an error.
            if let Job::Drained(done) = job {
                let _ = done.send(());
            }
            continue;
        };

        if let Some(why) = &poisoned {
            LineSink::remember_unwritten(&unwritten, &who, io::Error::other(why.clone()));
            LineSink::leave_pending(&pending, &who);
            freed.notify_waiters();
            let _ = done.send(());
            continue;
        }

        // Where the file ended before this record. `write_all` is a retry
        // loop, not an atomic commit: a filesystem can take a prefix and then
        // fail with `ENOSPC`, and the prefix would make every later record
        // unparseable. Truncating back to here is what keeps the file a
        // sequence of whole records even when a write fails partway.
        let before = match file.metadata().await {
            Ok(meta) => Some(meta.len()),
            Err(_) => None,
        };
        let written = async {
            file.write_all(&line).await?;
            file.flush().await
        }
        .await;

        if let Err(e) = written {
            // Rolling back is what keeps the file a sequence of whole records.
            // When it cannot be done -- the length before the append was never
            // read, or the truncation itself failed -- a prefix may be sitting
            // there, and appending the next record to it would produce a line
            // nothing can parse. There is no recovering from that by writing
            // more, so the writer stops: every later record is reported
            // unwritten rather than added to a file already broken.
            // Why recovery could not be proved, kept rather than collapsed to
            // a boolean: if no further record ever arrives, this is the only
            // thing that will tell a reader the file may be corrupt and what
            // stopped it being repaired.
            let recovery = match before {
                None => Some("the file's length before the record was never read".to_string()),
                Some(before) => match file.set_len(before).await {
                    Err(e) => Some(format!("truncating back to {before} failed: {e}")),
                    Ok(()) => match file.flush().await {
                        Err(e) => Some(format!("flushing the truncation failed: {e}")),
                        Ok(()) => None,
                    },
                },
            };
            (labels.warn)(format_args!("{what} write failed: {e}"));
            LineSink::remember_unwritten(&unwritten, &who, e);
            if let Some(why) = recovery {
                (labels.error)(format_args!(
                    "the {what} log may hold a partial record and cannot be recovered ({why}); \
                     no further records will be written to it"
                ));
                let integrity = format!(
                    "the {what} log may hold a partial record that could not be rolled back \
                     ({why}), so nothing further may be appended to it"
                );
                // Replaces the write error this record already recorded
                // rather than counting a second loss: the same record, and
                // this is the more useful thing to be told about it. Recorded
                // now so a teardown that follows immediately still learns the
                // file is suspect, even if nothing else is ever written.
                LineSink::remember_integrity(&unwritten, &who, &integrity);
                poisoned = Some(integrity);
            }
        }
        // Answered whatever the outcome: the producer is waiting to learn the
        // record has been dealt with, and a failure it can read back from the
        // losses is dealt with. Released for the same reason -- this record
        // has been accounted for one way or the other, so it is no longer one
        // the writer is holding.
        LineSink::leave_pending(&pending, &who);
        freed.notify_waiters();
        let _ = done.send(());
    }
}

/// What the writer shares with the sink's handles.
struct Parts<K> {
    pending: Pending<K>,
    unwritten: Losses<K>,
    freed: Arc<Notify>,
    labels: &'static Labels,
}

#[cfg(test)]
#[path = "line_sink_tests.rs"]
mod tests;
