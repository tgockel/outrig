use std::path::PathBuf;

use super::*;

static LABELS: Labels = Labels {
    what: "test record",
    claimant: "test",
    warn: |_| {},
    error: |_| {},
};

async fn open(path: &Path, perm: Option<u32>) -> LineSink<&'static str> {
    LineSink::open(path, perm, QUEUE, &LABELS)
        .await
        .expect("open the sink")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

/// A `stat` that failed is not an answer, and must not be read as one.
/// Treating it like a device -- which is what "not a regular file" would
/// mean -- spawns a writer that truncates with nothing claiming the file,
/// which is the pairing the claim exists to prevent.
#[test]
fn a_file_this_cannot_identify_is_not_taken_for_a_device() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.jsonl");
    std::fs::write(&path, b"").expect("create");

    assert!(
        claims_exclusively(std::fs::metadata(&path)).expect("a readable file"),
        "a regular file is claimed"
    );
    assert!(
        !claims_exclusively(std::fs::metadata("/dev/full")).expect("a readable device"),
        "and a device is not: there is no truncation to protect"
    );
    assert!(
        claims_exclusively(Err(io::Error::from(io::ErrorKind::PermissionDenied))).is_err(),
        "and a file this could not identify is neither"
    );
}

/// A mode asked for is the mode the file has, whatever the umask, and
/// whatever mode a file already there had.
#[tokio::test]
async fn a_file_gets_the_mode_it_was_opened_with() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fresh = dir.path().join("fresh.jsonl");
    let mut sink = open(&fresh, Some(0o600)).await;
    sink.close().await;
    assert_eq!(mode(&fresh), 0o600, "a file it creates");

    let existing = dir.path().join("existing.jsonl");
    std::fs::write(&existing, b"").expect("create");
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let mut sink = open(&existing, Some(0o600)).await;
    sink.close().await;
    assert_eq!(mode(&existing), 0o600, "and one that was there, narrowed");

    let untouched = dir.path().join("untouched.jsonl");
    std::fs::write(&untouched, b"").expect("create");
    std::fs::set_permissions(&untouched, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let mut sink = open(&untouched, None).await;
    sink.close().await;
    assert_eq!(
        mode(&untouched),
        0o644,
        "and none asked for changes nothing"
    );

    // A device is not a file anything narrows: changing `/dev/full` would
    // take privileges this does not have, and is not this sink's to do.
    let mut sink = open(&PathBuf::from("/dev/full"), Some(0o600)).await;
    sink.close().await;
}

/// The words a refusal uses are the caller's: a second writer is told whose
/// log it is.
#[tokio::test]
async fn a_second_writer_is_refused_in_the_logs_own_words() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.jsonl");
    let _first = open(&path, None).await;
    let refused = LineSink::<&'static str>::open(&path, None, QUEUE, &LABELS)
        .await
        .expect_err("one writer owns it")
        .to_string();
    assert!(
        refused.contains("the test record log") && refused.contains("another test ("),
        "{refused}"
    );
}

/// What `write` returns after is in the file; what `enqueue` returns after
/// is queued, and answered once it is.
#[tokio::test]
async fn a_record_is_in_the_file_once_it_is_answered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.jsonl");
    let mut sink = open(&path, None).await;

    sink.write("a", b"{\"n\":1}\n".to_vec())
        .await
        .expect("written");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"n\":1}\n");

    let answered = sink
        .enqueue("b", b"{\"n\":2}\n".to_vec())
        .await
        .expect("queued");
    answered.await.expect("answered");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "{\"n\":1}\n{\"n\":2}\n"
    );

    assert!(sink.close().await.is_none(), "it finishes");
    assert!(sink.take_every_loss().is_empty(), "having lost nothing");
}

/// A failed append is a loss filed under the record's owner, with what broke
/// it, and once the rollback cannot be proved, what that did to the file.
#[tokio::test]
async fn a_loss_is_its_owners_with_its_cause_and_its_integrity() {
    let sink = open(&PathBuf::from("/dev/full"), None).await;
    sink.write("a", b"{}\n".to_vec()).await.expect("answered");
    sink.write("a", b"{}\n".to_vec()).await.expect("answered");
    sink.write("b", b"{}\n".to_vec()).await.expect("answered");

    let Loss {
        records,
        source,
        integrity,
    } = sink.take_loss(&"a").expect("a's loss");
    assert_eq!(records, 2);
    assert!(
        source.to_string().contains("space") || source.to_string().contains("full"),
        "{source}"
    );
    assert!(integrity.is_some(), "the rollback could not be proved");
    assert!(sink.take_loss(&"a").is_none(), "taken once");
    let rest: Vec<_> = sink
        .take_every_loss()
        .into_iter()
        .map(|(who, loss)| (who, loss.records))
        .collect();
    assert_eq!(rest, [("b", 1)]);
}

/// A write the runtime's blocking pool refuses because it is shutting down is
/// a write failure like any other -- not a panic on a worker thread. This is
/// tokio-rs/tokio#8182's trigger reproduced directly: `shutdown_background`
/// tears the pool down without waiting, and `handle.enter()` lets a future
/// still be polled by hand afterward, the only way to observe what
/// `spawn_blocking` does once nothing is left to run it. A `#[tokio::test]`
/// runtime cannot reach this shape -- it cannot be shut down out from under
/// the test that is still running on it -- so this drives `write_lines` with
/// a borrowed `Waker` instead of a second runtime driving it as a task.
#[test]
fn a_write_refused_by_a_shutting_down_runtime_is_reported_not_panicked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("log.jsonl");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("create the log");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let handle = rt.handle().clone();
    rt.shutdown_background();
    let _guard = handle.enter();

    // The sink only lends its accounting: the writer reads its own queue.
    let (unused, _) = mpsc::channel(1);
    let sink = LineSink::queuing_to(unused, None, &LABELS);
    let (records, queue) = mpsc::channel(1);
    let (done, mut answered) = oneshot::channel();
    records
        .try_send(Job::Record {
            who: "a",
            line: b"{}\n".to_vec(),
            done,
        })
        .expect("the channel has room for one, synchronously");
    // Nothing else is ever queued, so the writer's next `recv` sees the
    // channel closed once this record is dealt with and the loop ends.
    drop(records);

    let mut writer = std::pin::pin!(write_lines(file, queue, sink.clone_parts()));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    // Driven by hand rather than awaited: there is no executor left to hand
    // this to, and that absence is the point. A bound here is a failing test,
    // not impatience -- a writer that is actually stuck leaves
    // `Poll::Pending` forever, and spinning on that silently would only hide
    // it.
    let mut polls = 0;
    while std::future::Future::poll(writer.as_mut(), &mut cx).is_pending() {
        polls += 1;
        assert!(
            polls < 10_000,
            "the writer never finished with its one record"
        );
    }

    let lost = sink
        .take_loss(&"a")
        .map(|loss| (loss.records, loss.integrity.is_some()));
    assert_eq!(
        lost,
        Some((1, true)),
        "a refused spawn is a write failure, reported like any other, and the file is lost with it"
    );
    assert!(
        answered.try_recv().is_ok(),
        "the producer waiting on `done` is still answered, not left hanging"
    );
}

/// A writer that will not stop is stopped at the deadline, and what it had
/// taken and not answered for is counted, by owner, with the file's integrity
/// in doubt.
#[tokio::test(start_paused = true)]
async fn a_writer_stopped_at_its_deadline_reports_what_it_held() {
    let (records, _queue) = mpsc::channel(QUEUE);
    let mut sink =
        LineSink::queuing_to(records, Some(tokio::spawn(std::future::pending())), &LABELS);
    for who in ["a", "a", "b"] {
        let _answered = sink.enqueue(who, b"{}\n".to_vec()).await.expect("queued");
    }

    let started = tokio::time::Instant::now();
    let closed = sink.close().await.expect("a writer that will not stop");
    assert!(closed.to_string().contains("unaccounted"), "{closed}");
    assert_eq!(
        started.elapsed(),
        SHUTDOWN_GRACE,
        "and not a moment past it"
    );

    let counted: Vec<_> = sink
        .take_every_loss()
        .into_iter()
        .map(|(who, loss)| (who, loss.records, loss.integrity.is_some()))
        .collect();
    assert_eq!(counted, [("a", 2, true), ("b", 1, true)]);
}

/// A record offered without waiting is queued while there is room, and lost
/// -- counted as its owner's, with why -- once there is none; a caller that
/// waits for room is held until the writer catches up.
#[tokio::test]
async fn a_full_queue_counts_what_it_cannot_take_and_holds_who_waits() {
    let (records, mut queue) = mpsc::channel(2);
    let sink = LineSink::queuing_to(records, None, &LABELS);
    let room = sink.room();
    for n in 0..3 {
        let queued = sink.try_enqueue("a", format!("{n}\n").into_bytes(), || "full".to_string());
        assert_eq!(queued, n < 2, "record {n}");
    }
    let lost = sink.take_loss(&"a").expect("one lost");
    assert_eq!(
        (lost.records, lost.source.to_string()),
        (1, "full".to_string())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), room.below(2))
            .await
            .is_err(),
        "two held is not below two"
    );

    // What a writer does with one: takes it, and answers for it.
    let Some(Job::Record { who, .. }) = queue.recv().await else {
        panic!("a record");
    };
    LineSink::leave_pending(&sink.pending, &who);
    sink.freed.notify_waiters();
    within(room.below(2)).await;
}

async fn within<F: std::future::Future>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), f)
        .await
        .expect("in time")
}
