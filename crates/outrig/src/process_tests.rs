//! Unit tests for `process`: covers all four call patterns (`run_capture`,
//! `try_capture_logged`, `run_streamed`, `spawn_stdio`), the structured
//! `Process` error variant, the spawn/exit tracing every podman invocation
//! relies on, the honest stderr-tail truncation behavior, and both halves of
//! the ownership guarantee -- the bound a dropped future gets and the
//! confirmed reap a stop signal gets.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;

use crate::error::OutrigError;

use super::{Cmd, Termination, Transcript};

/// Ceiling for the drop path. Termination is synchronous and the reap is one
/// task hop, so the real figure is microseconds; this is margin for a loaded
/// CI runner, not a claim about the mechanism. Each test prints what it
/// actually measured.
const REAP_CEILING: Duration = Duration::from_secs(5);

#[test]
fn cmd_render_quotes_args_for_display() {
    let rendered = Cmd::new("podman")
        .arg("exec")
        .arg("hello world")
        .arg("it's")
        .arg("")
        .render();
    assert_eq!(rendered, "podman exec 'hello world' 'it'\\''s' ''");
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_echo_succeeds() {
    let out = super::run_capture(Cmd::new("/bin/echo").arg("hi"))
        .await
        .expect("/bin/echo hi must succeed");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hi\n");
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_false_fails_with_exit_1() {
    let err = super::run_capture(Cmd::new("/bin/false").arg("ignored-arg"))
        .await
        .expect_err("/bin/false must fail");
    let OutrigError::Process {
        program,
        argv,
        exit_code,
        stderr_tail: _,
    } = &err
    else {
        panic!("expected OutrigError::Process, got: {err:?}");
    };
    assert_eq!(*program, "/bin/false");
    assert_eq!(argv, &vec![OsString::from("ignored-arg")]);
    assert_eq!(*exit_code, Some(1));

    let rendered = format!("{err}");
    assert!(
        rendered.contains("/bin/false"),
        "Display must mention program, got: {rendered}"
    );
    assert!(
        rendered.contains("code 1"),
        "Display must mention exit code, got: {rendered}"
    );
    assert!(
        rendered.contains("argv:"),
        "Display must mention argv, got: {rendered}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn try_capture_returns_output_on_nonzero_exit() {
    let out = super::try_capture(Cmd::new("/bin/false"))
        .await
        .expect("try_capture must not error on non-zero exit");
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(1));
}

#[tokio::test(flavor = "current_thread")]
async fn logged_capture_tees_command_and_output_to_transcript() {
    // `run_capture_logged` reaches the same two callsites
    // `try_capture_logged_traces_spawn_and_exit_at_debug` asserts on, with no
    // subscriber installed. See `TRACING_CALLSITES`.
    let _emitting = emitting().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("container.log");
    let transcript = Transcript::create(&path, false)
        .await
        .expect("create transcript");

    let output = super::run_capture_logged(
        Cmd::new("/bin/sh").args(["-c", "echo out; echo err 1>&2"]),
        "test",
        Some(&transcript),
    )
    .await
    .expect("logged command succeeds");

    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");

    let log = std::fs::read_to_string(&path).expect("read transcript");
    assert!(log.contains("[test] $ /bin/sh -c"), "log was:\n{log}");
    assert!(log.contains("[test] out"), "log was:\n{log}");
    assert!(log.contains("[test] err"), "log was:\n{log}");
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_truncates_long_stderr_with_marker() {
    // Emit enough stderr to exceed the process tail limit.
    let cmd = Cmd::new("/bin/sh").args([
        "-c",
        "printf 'line-1\\n' 1>&2; \
         dd if=/dev/zero bs=1024 count=1025 1>&2 2>/dev/null; \
         printf 'line-5000\\n' 1>&2; \
         exit 1",
    ]);
    let err = super::run_capture(cmd)
        .await
        .expect_err("non-zero exit must fail");
    let OutrigError::Process { stderr_tail, .. } = &err else {
        panic!("expected OutrigError::Process, got: {err:?}");
    };
    assert!(
        stderr_tail.starts_with("... (truncated) ..."),
        "tail must announce truncation, got start: {:?}",
        &stderr_tail[..stderr_tail.len().min(40)]
    );
    assert!(
        !stderr_tail.contains("line-1\n"),
        "earliest line must be elided",
    );
    assert!(
        stderr_tail.contains("line-5000"),
        "latest line must be retained, got: {stderr_tail}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_keeps_large_stderr_tail_bounded() {
    let marker = "... (truncated) ...\n";
    let cmd = Cmd::new("/bin/sh").args([
        "-c",
        "dd if=/dev/zero bs=1024 count=10240 1>&2 2>/dev/null; \
         printf 'the-end\\n' 1>&2; \
         exit 1",
    ]);
    let err = super::run_capture(cmd)
        .await
        .expect_err("non-zero exit must fail");
    let OutrigError::Process { stderr_tail, .. } = &err else {
        panic!("expected OutrigError::Process, got: {err:?}");
    };

    assert!(
        stderr_tail.starts_with(marker),
        "tail must announce truncation, got start: {:?}",
        &stderr_tail[..stderr_tail.len().min(40)]
    );
    assert!(
        stderr_tail.len() <= marker.len() + 1024 * 1024,
        "tail should stay bounded near 1 MiB, got {} bytes",
        stderr_tail.len()
    );
    assert!(
        stderr_tail.contains("the-end"),
        "latest stderr must be retained, got tail length {}",
        stderr_tail.len()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_does_not_mark_exact_limit_stderr_truncated() {
    let marker = "... (truncated) ...\n";
    let cmd = Cmd::new("/bin/sh").args([
        "-c",
        "dd if=/dev/zero bs=1024 count=1024 1>&2 2>/dev/null; exit 1",
    ]);
    let err = super::run_capture(cmd)
        .await
        .expect_err("non-zero exit must fail");
    let OutrigError::Process { stderr_tail, .. } = &err else {
        panic!("expected OutrigError::Process, got: {err:?}");
    };

    assert!(
        !stderr_tail.starts_with(marker),
        "exact-limit stderr should not be marked truncated"
    );
    assert_eq!(stderr_tail.len(), 1024 * 1024);
}

#[test]
fn run_streamed_forwards_stderr_to_tracing() {
    let (status, captured) = with_captured_tracing_at(tracing::Level::TRACE, async {
        super::run_streamed(
            Cmd::new("/bin/sh").args(["-c", "echo hello-from-stderr 1>&2"]),
            "test",
            Termination::Kill,
        )
        .await
        .expect("run_streamed must succeed")
    });
    assert!(status.success());
    assert!(
        captured.contains("[test] hello-from-stderr"),
        "tracing should receive prefixed stderr line, got: {captured}"
    );
}

#[test]
fn try_capture_logged_traces_spawn_and_exit_at_debug() {
    let ((), captured) = with_captured_tracing_at(tracing::Level::DEBUG, async {
        super::try_capture_logged(Cmd::new("/bin/echo").arg("hi"), "test", None)
            .await
            .expect("try_capture_logged must succeed");
    });
    assert!(
        captured.contains("spawn command=/bin/echo hi"),
        "debug output should name the full command line, got: {captured}"
    );
    assert!(
        captured.contains("program=\"/bin/echo\"")
            && captured.contains("code=Some(0)")
            && captured.contains("elapsed_ms=")
            && captured.contains("exit"),
        "debug output should record exit code and elapsed time, got: {captured}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_stdio_stdin_stdout_usable() {
    let mut child = super::spawn_stdio(Cmd::new("/bin/cat"))
        .await
        .expect("spawn /bin/cat must succeed");
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let mut stdout = child.stdout.take().expect("stdout was piped");

    stdin
        .write_all(b"round-trip\n")
        .await
        .expect("write to cat stdin");
    drop(stdin);

    let mut buf = Vec::new();
    stdout
        .read_to_end(&mut buf)
        .await
        .expect("read from cat stdout");
    assert_eq!(buf, b"round-trip\n");

    let status = child.wait().await.expect("wait on cat");
    assert!(status.success());
}

/// Separates the tests that *observe* tracing from the tests that merely
/// *emit* through the same callsites.
///
/// `tracing` keeps callsite state process-globally while
/// `subscriber::set_default` installs a subscriber on one thread, so a test
/// that observes tracing can capture nothing at all when it runs beside one
/// that merely emits through the same module's callsites.
///
/// The mechanism is not pinned down past that, and saying so is deliberate:
/// two earlier diagnoses of this flake did not survive being checked against
/// the source, and the one-line remedy the first of them implied measured the
/// same as no remedy at all. What is established is measured, on the compiled
/// lib binary:
///
/// | Configuration                                   | Failures |
/// |-------------------------------------------------|----------|
/// | Before this gate                                | 6 / 40   |
/// | Observers serialized against each other only    | 12 / 50  |
/// | Every guard but the `run_streamed` emitter's    | 0 / 40   |
/// | This gate                                       | 0 / 140  |
///
/// Every failure in every row was
/// `run_streamed_forwards_stderr_to_tracing`. Two rows carry the finding.
/// Serializing the observers against *each other* does not help and reads
/// worse than doing nothing, so what matters is excluding the emitters rather
/// than ordering the observers. And the emitters that matter are the
/// `try_capture_logged` ones, which never touch the callsite the failing test
/// asserts on -- so whatever is shared here, it is not that callsite's own
/// `Interest`.
///
/// The fifth guard is kept even though the third row says it is not what
/// closes this. It sits on the only other caller of the callsite the failing
/// test asserts on, which is where a future change would most plausibly make
/// it matter, and it costs nothing measurable.
///
/// An `RwLock` rather than a `Mutex` because the asymmetry is the point.
/// Observers take the write side and so exclude everyone; emitters take the
/// read side and still run concurrently with each other, which is all but
/// three tests in this file.
///
/// The obligation this encodes is real, and wider than this file: a test that
/// reaches `try_capture_logged*`, `run_capture_logged*` or `run_streamed`
/// without a subscriber wants `emitting()`, and most of them reach it
/// *indirectly*. The ones outside this file today are `network`'s tests, all
/// of which run their commands through `tests::run_step_gated`, and the two
/// `container` tests that call `Container::stop`. A review caught both after
/// the first version of this gate guarded only the direct callers here.
///
/// Take it in the test, never inside the production helper: an observer holds
/// the write side while calling those same helpers, so a read acquired
/// underneath it would deadlock.
///
/// `plan/next/relocate-unit-shaped-tests.md` is where the version that needs
/// no obligation lives -- these assert on `pub(crate)` items, so giving them
/// their own process means widening the crate's surface, which is not a thing
/// to do during a release freeze.
/// `tokio`'s rather than `std`'s: an emitter holds the read side across the
/// `await` that reaches the callsite, which a `std` guard may not do, and this
/// one does not poison -- a test failing while it holds either side would
/// otherwise turn one failure into every failure.
static TRACING_CALLSITES: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// The read side: a test that drives a traced callsite without installing a
/// subscriber.
pub(crate) async fn emitting() -> tokio::sync::RwLockReadGuard<'static, ()> {
    TRACING_CALLSITES.read().await
}

/// The write side: a test that installs a subscriber and asserts on what it
/// captured.
///
/// `blocking_write` rather than an `await`, because every observer is a plain
/// `#[test]` that builds its runtime *after* taking this -- so there is no
/// runtime on the thread to block, which is also what keeps the subscriber and
/// the work it observes on one thread.
fn observing() -> tokio::sync::RwLockWriteGuard<'static, ()> {
    TRACING_CALLSITES.blocking_write()
}

/// Run `body` under a capturing subscriber at `level`, returning its value and
/// everything the subscriber recorded.
///
/// Holds [`observing`] throughout, which is what makes the capture meaningful.
/// The runtime is current-thread and built here rather than by
/// `#[tokio::test]` because `set_default` is thread-local: the work -- and any
/// task it spawns -- has to be polled on the thread holding the guard.
pub(crate) fn with_captured_tracing_at<T>(
    level: tracing::Level,
    body: impl Future<Output = T>,
) -> (T, String) {
    let _observing = observing();

    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(buf.clone()))
        .with_max_level(level)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");
    let out = rt.block_on(body);

    let captured = String::from_utf8(buf.lock().unwrap().clone())
        .expect("captured tracing output must be UTF-8");
    (out, captured)
}

/// A `tracing` writer that keeps what was written, so a test can assert on a
/// diagnostic rather than on the state the diagnostic describes. Shared with
/// `mcp_proxy_dispatch_tests`.
#[derive(Clone, Default)]
pub(crate) struct CaptureWriter(pub(crate) Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// Ownership: what happens to the child when nobody is waiting for it
// ---------------------------------------------------------------------------

/// A child that publishes its own pid and then blocks for far longer than any
/// test will wait.
///
/// `exec` matters: without it the shell would fork `sleep` as a *grandchild*,
/// and killing the shell would orphan a live sleeper into the developer's
/// session. With it the shell becomes `sleep`, keeping the pid it just wrote,
/// so the pid these tests assert on is the process that is actually asleep.
///
/// 30 seconds is well past `REAP_CEILING`, so a sleeper never exits on its own
/// and lets an assertion pass for the wrong reason, and short enough that a
/// failing run still cleans up after itself.
fn pid_publishing_sleeper(pid_file: &Path) -> Cmd {
    Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!("echo $$ > {}; exec sleep 30", pid_file.display()),
    ])
}

/// Wait until the sleeper has published its pid. The child is a real process,
/// so it makes progress whether or not the future holding it is being polled
/// -- which is what lets a test learn the pid without driving the helper.
async fn published_pid(pid_file: &Path) -> u32 {
    let deadline = Instant::now() + REAP_CEILING;
    loop {
        if let Ok(text) = std::fs::read_to_string(pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the child never published a pid");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// Whether `pid` still occupies a slot in the process table.
///
/// `kill(pid, 0)` succeeds for a **zombie** as well as a running process, and
/// these children are direct children of the test binary, so only outrig's own
/// reap can clear them. That makes this one call the whole assertion: false
/// means terminated *and* reaped, which is what "no zombie" has to mean here.
///
/// Pid reuse would confuse this, but Linux allocates pids monotonically until
/// it wraps, so it cannot happen inside the milliseconds these tests span.
fn occupies_a_process_slot(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Poll until `pid` leaves the process table, returning how long it took.
///
/// Polled rather than asserted instantaneously: `Drop` cannot await, so the
/// drop path can promise a bound and not an immediate reap. `async`, and
/// `tokio::time::sleep` rather than `thread::sleep`, because the reap is a
/// task -- a test that blocked its runtime thread here would be preventing
/// the very thing it is waiting for.
async fn elapsed_until_gone(pid: u32) -> Duration {
    let started = Instant::now();
    while occupies_a_process_slot(pid) {
        assert!(
            started.elapsed() < REAP_CEILING,
            "pid {pid} still occupied a process slot after {REAP_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    started.elapsed()
}

/// Whether `pid` has stopped, counting a zombie as stopped.
///
/// The bar for a *descendant* outrig deliberately orphaned, and lower than the
/// one for outrig's own children on purpose. Such a process exits, is
/// reparented to pid 1, and leaves the table only when pid 1 reaps it -- which
/// a container whose pid 1 is a shell rather than an init never does. The
/// writer would then sit in `Z` forever and a disappearance test would fail
/// for a reason that has nothing to do with the drain it is about, on a
/// runner where outrig had behaved perfectly. What these tests need from a
/// descendant is that it stopped writing, and a zombie has.
///
/// Outrig's own children stay held to [`elapsed_until_gone`]: reaping them is
/// the guarantee under test, so a zombie there is exactly the failure.
fn has_stopped(pid: u32) -> bool {
    !occupies_a_process_slot(pid) || is_zombie(pid)
}

fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state character follows the comm field, which is parenthesized and
    // may itself contain spaces and parentheses -- so split at the *last*
    // `)` rather than counting whitespace-separated fields from the left.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return false;
    };
    after_comm.split_whitespace().next() == Some("Z")
}

/// The zombie half of [`has_stopped`], against a real one.
///
/// Worth its own test because the case it exists for never arises on a host
/// with a reaping pid 1 -- so the parse would otherwise be exercised only on
/// the machines where getting it wrong is expensive, and a `/proc` field
/// counted from the wrong end fails silently by reporting "not a zombie".
#[test]
fn a_zombie_counts_as_stopped_but_still_occupies_a_slot() {
    // `std`, not tokio: a tokio `Child` is reaped by the runtime's orphan
    // queue, and this needs a child nobody reaps.
    let mut child = std::process::Command::new("/bin/true")
        .spawn()
        .expect("spawn /bin/true");
    let pid = child.id();

    let deadline = Instant::now() + REAP_CEILING;
    while !is_zombie(pid) {
        assert!(
            Instant::now() < deadline,
            "the child never became a zombie; nothing here reaps it, so \
             either it did not exit or the /proc parse is wrong"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    assert!(
        occupies_a_process_slot(pid),
        "a zombie is still in the process table -- that is the whole reason \
         the disappearance bar is wrong for a descendant"
    );
    assert!(has_stopped(pid), "a zombie has stopped");

    child.wait().expect("reap the zombie");
    assert!(!is_zombie(pid), "reaped, so no longer a zombie");
}

/// Poll until `pid` has stopped in the sense [`has_stopped`] describes.
async fn elapsed_until_stopped(pid: u32) -> Duration {
    let started = Instant::now();
    while !has_stopped(pid) {
        assert!(
            started.elapsed() < REAP_CEILING,
            "pid {pid} was still running after {REAP_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    started.elapsed()
}

fn pid_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("pid")
}

/// The drop path: a future cancelled mid-command leaves no process behind.
/// This is the audit's repro, at the level the helpers live on.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_capture_kills_and_reaps_its_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let mut capture = Box::pin(super::run_capture(pid_publishing_sleeper(&path)));
    // Drop only once the child demonstrably exists; a timer here would be
    // racing the spawn rather than testing the drop.
    let pid = tokio::select! {
        _ = &mut capture => panic!("the sleeper exited on its own"),
        pid = published_pid(&path) => pid,
    };
    assert!(
        occupies_a_process_slot(pid),
        "the sleeper should be running"
    );

    drop(capture);

    let took = elapsed_until_gone(pid).await;
    println!("drop path: pid {pid} gone in {took:?} (ceiling {REAP_CEILING:?})");
}

/// The same drop path reached the way a caller actually reaches it -- through
/// `tokio::time::timeout`, which drops the future it wrapped.
#[tokio::test(flavor = "current_thread")]
async fn a_timed_out_capture_kills_and_reaps_its_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let outcome = tokio::time::timeout(
        Duration::from_millis(250),
        super::run_capture(pid_publishing_sleeper(&path)),
    )
    .await;
    assert!(
        outcome.is_err(),
        "the sleeper should have outlived the budget"
    );

    let pid = published_pid(&path).await;
    let took = elapsed_until_gone(pid).await;
    println!("timeout path: pid {pid} gone in {took:?} (ceiling {REAP_CEILING:?})");
}

/// Cancellation injected at the spawn-to-owner handoff itself, and
/// deterministically rather than by racing a timer.
///
/// One poll drives the helper from entry through `spawn_owned` to its first
/// `.await`, which is the child's `wait`. That is the entire window in which
/// the process exists and the enclosing future has not yet been suspended --
/// `spawn_owned` is synchronous, so there is no *earlier* point a drop could
/// land, and this test is what pins that.
#[tokio::test(flavor = "current_thread")]
async fn a_drop_at_the_spawn_handoff_leaves_no_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let mut capture = Box::pin(super::run_capture(pid_publishing_sleeper(&path)));
    let first = futures_util::poll!(&mut capture);
    assert!(first.is_pending(), "the sleeper cannot have finished");

    // The child runs on its own; nothing polls `capture` between here and the
    // drop below, which is exactly the state being tested.
    let pid = published_pid(&path).await;
    drop(capture);

    let took = elapsed_until_gone(pid).await;
    println!("handoff window: pid {pid} gone in {took:?} (ceiling {REAP_CEILING:?})");
}

/// A descendant that inherited the child's stdout does not keep the drain
/// alive after the call is abandoned.
///
/// Killing the direct child does not close the pipe: anything it left behind
/// holding that descriptor keeps the write end open, and a detached drain task
/// would go on reading from it -- and go on growing an unbounded buffer -- for
/// as long as the descendant lived. Aborting the drain closes the read end,
/// and the descendant learns that by dying of `EPIPE` on its next write, which
/// is what this observes.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_capture_does_not_leave_its_drain_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let descendant_file = dir.path().join("descendant");

    // The backgrounded writer inherits stdout and outlives the shell, which
    // `exec`s into a sleeper. `$!` is the writer's own pid -- `$$` inside a
    // subshell is the *parent's*, which would make this assert on the process
    // the drop kills directly and pass for the wrong reason.
    let cmd = Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "while : ; do echo tick || exit 0; sleep 0.02; done & echo $! > {}; exec sleep 30",
            descendant_file.display()
        ),
    ]);

    let mut capture = Box::pin(super::run_capture(cmd));
    let descendant = tokio::select! {
        _ = &mut capture => panic!("the sleeper exited on its own"),
        pid = published_pid(&descendant_file) => pid,
    };
    assert!(
        occupies_a_process_slot(descendant),
        "the descendant should be writing"
    );

    drop(capture);

    let took = elapsed_until_stopped(descendant).await;
    println!("descendant with the inherited pipe: gone in {took:?}");
}

/// The same leak, reached through the *join* rather than through the child's
/// `wait`: the direct child exits promptly and a descendant keeps the pipe
/// open, so the helper is parked draining a stream nobody will close.
///
/// This is where a cancellation is most likely to land, and where taking the
/// `JoinHandle` out of its guard before awaiting would detach the drain rather
/// than abort it.
///
/// Reaching that state has to be *proven*, not timed. The shell publishes its
/// own pid, and only the helper's `Owned::wait` can reap it -- so once that pid
/// has left the process table while the helper is still pending, the helper has
/// necessarily returned from `wait` and is parked in the join. A marker written
/// just before the shell exits would not show that: it becomes visible while
/// the helper may still be waiting, which would let this pass with the bug
/// back.
#[tokio::test(flavor = "current_thread")]
async fn a_capture_canceled_while_draining_does_not_detach_the_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let descendant_file = dir.path().join("descendant");
    let direct_file = dir.path().join("direct");

    let cmd = Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "while : ; do echo tick || exit 0; sleep 0.02; done & echo $! > {}; echo $$ > {}",
            descendant_file.display(),
            direct_file.display()
        ),
    ]);

    let mut capture = Box::pin(super::run_capture(cmd));
    // The first poll is what spawns; nothing exists to wait for before it.
    assert!(futures_util::poll!(&mut capture).is_pending());
    let descendant = published_pid(&descendant_file).await;
    let direct = published_pid(&direct_file).await;

    let deadline = Instant::now() + REAP_CEILING;
    loop {
        assert!(
            futures_util::poll!(&mut capture).is_pending(),
            "the capture cannot finish while the descendant holds the pipe"
        );
        // Gone means reaped, and only the helper reaps it -- so this is the
        // moment `wait` returned and the join began.
        if !occupies_a_process_slot(direct) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the direct child was never reaped, so the helper never reached \
             the join this test is about"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    drop(capture);

    let took = elapsed_until_stopped(descendant).await;
    println!("descendant, canceled mid-drain: gone in {took:?}");
}

/// The cooperative twin of the test above, in the same state: the direct child
/// exits on time and a descendant holds the pipe, so the helper is parked in
/// the join rather than in the wait.
///
/// A stop signal retired at the child's exit would leave this stretch
/// unbounded, and it is the stretch a budget is least able to do without: the
/// command has already finished, so a caller that hangs here is waiting on
/// nothing it asked for. `Container::stop` is the caller, and hanging there is
/// the wedged-teardown its budget exists to prevent.
///
/// The descendant is the assertion rather than the return alone. Its writes
/// are `echo tick || exit 0`, so it survives exactly as long as something
/// holds the read end open -- and it going away proves both drains were
/// *aborted* by the stop, not detached to run on unobserved.
#[tokio::test(flavor = "current_thread")]
async fn a_stop_while_draining_returns_and_aborts_the_readers() {
    let _emitting = emitting().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let descendant_file = dir.path().join("descendant");
    let direct_file = dir.path().join("direct");

    let cmd = Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "while : ; do echo tick || exit 0; sleep 0.02; done & echo $! > {}; echo $$ > {}",
            descendant_file.display(),
            direct_file.display()
        ),
    ]);

    let cancel = CancellationToken::new();
    let (result, descendant) = tokio::join!(
        super::try_capture_logged_until(
            cmd,
            "test",
            None,
            Termination::Kill,
            cancel.clone().cancelled_owned()
        ),
        async {
            let descendant = published_pid(&descendant_file).await;
            let direct = published_pid(&direct_file).await;
            // Gone means reaped, and only the helper reaps it -- so this is
            // the moment `wait` returned and the join began. Signalling on a
            // timer instead would let the stop land in the wait and prove
            // nothing about the stretch after it.
            let deadline = Instant::now() + REAP_CEILING;
            while occupies_a_process_slot(direct) {
                assert!(
                    Instant::now() < deadline,
                    "the direct child was never reaped, so the helper never \
                     reached the join this test is about"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            cancel.cancel();
            descendant
        },
    );

    assert!(
        matches!(result, Err(OutrigError::Canceled { .. })),
        "a stop landing in the drain must return, and as Canceled, got: {result:?}"
    );

    let took = elapsed_until_stopped(descendant).await;
    println!("descendant, stopped mid-drain: gone in {took:?}");
}

/// The cooperative contract at the same handoff window as above: the stop
/// signal is already ready when the helper first parks, so the cancel branch is
/// taken at the earliest instant it can be. Both contracts therefore hold
/// across the window -- a bounded termination for a dropped future, a confirmed
/// reap for a stopped one -- rather than only the drop half.
#[tokio::test(flavor = "current_thread")]
async fn a_stop_at_the_spawn_handoff_returns_after_a_confirmed_reap() {
    let _emitting = emitting().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = super::try_capture_logged_until(
        pid_publishing_sleeper(&path),
        "test",
        None,
        Termination::Kill,
        cancel.cancelled_owned(),
    )
    .await;

    assert!(
        matches!(result, Err(OutrigError::Canceled { .. })),
        "a stop at the handoff must report Canceled, got: {result:?}"
    );

    // The child may not have reached its `echo` before the kill landed, which
    // is the point: nothing outside the helper knew its pid yet, and it is gone
    // anyway. When it did get that far, the pid must be clear immediately.
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(pid) = text.trim().parse::<u32>()
    {
        assert!(
            !occupies_a_process_slot(pid),
            "pid {pid} was still in the process table when the call returned"
        );
    }
}

/// The cooperative path: when the caller supplies a stop signal, the child is
/// dead *and reaped* by the time the call returns. Asserted with no polling,
/// which is the difference from every test above.
#[tokio::test(flavor = "current_thread")]
async fn a_stopped_capture_returns_after_a_confirmed_reap() {
    let _emitting = emitting().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let cancel = CancellationToken::new();
    let (result, pid) = tokio::join!(
        super::try_capture_logged_until(
            pid_publishing_sleeper(&path),
            "test",
            None,
            Termination::Kill,
            cancel.clone().cancelled_owned()
        ),
        async {
            let pid = published_pid(&path).await;
            cancel.cancel();
            pid
        },
    );

    let Err(OutrigError::Canceled { program, argv }) = result else {
        panic!("a stopped capture must report Canceled, got: {result:?}");
    };
    assert_eq!(program, "/bin/sh");
    assert_eq!(argv[0], OsString::from("-c"));

    assert!(
        !occupies_a_process_slot(pid),
        "pid {pid} was still in the process table when the call returned; \
         the cooperative path promises a reap, not a kill in flight"
    );
}

/// `try_capture` is the shape behind the public `Container::exec_capture`, and
/// the one that used to be `Command::output()` -- a shorter call with exactly
/// the same hazard. Covered on its own because "it delegates to the same
/// abstraction" is a claim about the source, not a test.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_try_capture_kills_and_reaps_its_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let mut capture = Box::pin(super::try_capture(pid_publishing_sleeper(&path)));
    let pid = tokio::select! {
        _ = &mut capture => panic!("the sleeper exited on its own"),
        pid = published_pid(&path) => pid,
    };
    drop(capture);

    let took = elapsed_until_gone(pid).await;
    println!("try_capture drop: pid {pid} gone in {took:?} (ceiling {REAP_CEILING:?})");
}

/// `spawn_stdio` is the module's stated exception: the caller owns the child.
/// It is still kill-on-drop, so dropping the handle terminates the process --
/// the reap is what moves to the holder, not the kill.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_spawn_stdio_child_is_killed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let child = super::spawn_stdio(pid_publishing_sleeper(&path))
        .await
        .expect("spawn must succeed");
    let pid = published_pid(&path).await;
    assert!(
        occupies_a_process_slot(pid),
        "the sleeper should be running"
    );

    drop(child);

    let took = elapsed_until_gone(pid).await;
    println!("spawn_stdio drop: pid {pid} gone in {took:?} (ceiling {REAP_CEILING:?})");
}

/// A sleeper that publishes its pid, catches `SIGTERM`, records that it did,
/// and exits of its own accord.
///
/// Not `exec`ed, unlike [`pid_publishing_sleeper`]: a trap cannot survive an
/// `exec`, and catching the signal is the whole point here. `SIGKILL` cannot
/// be caught, so the marker existing is proof that what arrived was something
/// catchable rather than the escalation.
fn stop_catching_sleeper(pid_file: &Path, marker: &Path) -> Cmd {
    Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "trap 'echo caught > {}; exit 0' TERM; echo $$ > {}; sleep 5 & wait",
            marker.display(),
            pid_file.display()
        ),
    ])
}

/// A sleeper that publishes its pid and then ignores `SIGTERM`.
///
/// An ignored disposition survives `fork` and `exec`, so the backgrounded
/// `sleep` ignores it too and nothing short of the escalation ends this.
fn stop_ignoring_sleeper(pid_file: &Path) -> Cmd {
    Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "trap '' TERM; echo $$ > {}; sleep 5 & wait",
            pid_file.display()
        ),
    ])
}

/// The graceful drop path asks first. The marker proves a catchable signal
/// arrived, and finishing well inside the grace proves the child reached its
/// own ending rather than being killed at the deadline.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_graceful_child_is_asked_to_stop_before_it_is_killed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);
    let marker = dir.path().join("caught");

    let grace = Duration::from_secs(2);
    let child = stop_catching_sleeper(&path, &marker)
        .spawn_owned(
            super::StdioSpec::captured(),
            Termination::TermThenKill(grace),
        )
        .expect("spawn must succeed");
    let pid = published_pid(&path).await;
    drop(child);

    let took = elapsed_until_gone(pid).await;
    assert!(
        marker.exists(),
        "the child should have caught a signal it could catch"
    );
    assert!(
        took < grace,
        "the child ended at {took:?}, not inside the {grace:?} grace -- \
         that is the escalation firing, not the child stopping"
    );
    println!("graceful drop: pid {pid} stopped itself in {took:?} (grace {grace:?})");
}

/// A child that refuses the stop is still killed, and not before the grace is
/// up. The lower bound is the half that proves we waited rather than killing
/// at once and calling it graceful.
#[tokio::test(flavor = "current_thread")]
async fn a_graceful_child_that_ignores_the_stop_is_killed_at_the_grace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let grace = Duration::from_millis(150);
    let child = stop_ignoring_sleeper(&path)
        .spawn_owned(
            super::StdioSpec::captured(),
            Termination::TermThenKill(grace),
        )
        .expect("spawn must succeed");
    let pid = published_pid(&path).await;
    let dropped = Instant::now();
    drop(child);

    elapsed_until_gone(pid).await;
    let took = dropped.elapsed();
    assert!(
        took >= grace,
        "pid {pid} was gone in {took:?}, inside the {grace:?} grace it was owed"
    );
    println!("graceful escalation: pid {pid} gone in {took:?} (grace {grace:?})");
}

/// The awaited path runs the same sequence and returns only once the child is
/// terminated *and* reaped, matching
/// [`a_stopped_capture_returns_after_a_confirmed_reap`].
#[tokio::test(flavor = "current_thread")]
async fn a_graceful_terminate_returns_after_a_confirmed_reap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);
    let marker = dir.path().join("caught");

    let mut child = stop_catching_sleeper(&path, &marker)
        .spawn_owned(
            super::StdioSpec::captured(),
            Termination::TermThenKill(Duration::from_secs(2)),
        )
        .expect("spawn must succeed");
    let pid = published_pid(&path).await;
    child.terminate().await.expect("terminate must succeed");

    assert!(
        !occupies_a_process_slot(pid),
        "terminate returned with pid {pid} still in the process table"
    );
    assert!(
        marker.exists(),
        "terminate should ask a graceful child to stop before killing it"
    );
}

/// The bound 0002-39 established, pinned against this change: under the
/// default policy a child that ignores `SIGTERM` is still gone promptly,
/// because nothing asked it anything.
#[tokio::test(flavor = "current_thread")]
async fn the_default_policy_is_still_an_immediate_kill() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let child = stop_ignoring_sleeper(&path)
        .spawn_owned(super::StdioSpec::captured(), Termination::Kill)
        .expect("spawn must succeed");
    let pid = published_pid(&path).await;
    drop(child);

    let took = elapsed_until_gone(pid).await;
    assert!(
        took < Duration::from_secs(1),
        "the default policy waited {took:?} for a child that ignores SIGTERM"
    );
    println!("default drop: pid {pid} gone in {took:?}");
}

/// A stop is worth nothing to a child that cannot write.
///
/// The helpers hand each pipe to a [`Drain`], which aborts when dropped, and
/// a dropped future drops the drain first -- so without the keepalive the
/// read end is gone before the signal goes out and the child dies of
/// `SIGPIPE` on its first write instead of running its cleanup. The child
/// here writes from inside its handler, so the marker only appears if the
/// pipe was still open when the stop arrived.
///
/// Measured against buildah 1.33.7 before it was a test: a cancelled build
/// exited on signal 13 and left the working container it was asked to remove.
#[tokio::test(flavor = "current_thread")]
async fn a_graceful_child_can_still_write_while_it_stops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);
    let marker = dir.path().join("caught");

    let cmd = Cmd::new("/bin/sh").args([
        "-c".to_string(),
        format!(
            "trap 'echo stopping 1>&2; : > {}; exit 0' TERM; echo started 1>&2; \
             echo $$ > {}; sleep 5 & wait",
            marker.display(),
            path.display()
        ),
    ]);
    let mut child = cmd
        .spawn_owned(
            super::StdioSpec::streamed(),
            Termination::TermThenKill(Duration::from_secs(2)),
        )
        .expect("spawn must succeed");
    // Taken and dropped, which is what the helpers' aborted drains amount to
    // by the time the signal is sent.
    drop(child.take_stderr());
    let pid = published_pid(&path).await;
    drop(child);

    elapsed_until_gone(pid).await;
    assert!(
        marker.exists(),
        "the child could not write while stopping, so it died of SIGPIPE \
         instead of running its handler"
    );
}

/// A noisy child does not hang a graceful build.
///
/// The keepalive holds the pipe's read end open, so if the drain stops before
/// the child does, nothing is reading and the child blocks once the 64 KiB
/// pipe buffer fills -- with no cancellation in sight and no grace to end it.
/// Invalid UTF-8 on stderr used to do exactly that, because `lines()` yields
/// `Err` for it and the drain's loop ended there.
///
/// The child writes a bad byte and then far more than a pipe buffer's worth,
/// so a drain that gave up at the bad byte cannot reach the end.
#[tokio::test(flavor = "current_thread")]
async fn a_graceful_child_writing_invalid_utf8_still_finishes() {
    // Drives `run_streamed`'s stderr callsite -- the one
    // `run_streamed_forwards_stderr_to_tracing` captures -- 2001 times, with
    // no subscriber installed. See `TRACING_CALLSITES`.
    let _emitting = emitting().await;
    let cmd = Cmd::new("/bin/sh").args([
        "-c".to_string(),
        // `printf` writes the lone continuation byte 0x80, which is not
        // valid UTF-8 in any position.
        "printf '\\200\\n' 1>&2; i=0; while [ $i -lt 2000 ]; do \
         echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1>&2; \
         i=$((i+1)); done"
            .to_string(),
    ]);

    let status = tokio::time::timeout(
        Duration::from_secs(20),
        super::run_streamed(
            cmd,
            "test",
            Termination::TermThenKill(Duration::from_secs(5)),
        ),
    )
    .await
    .expect("the child blocked on an unread pipe instead of finishing")
    .expect("run_streamed must succeed");
    assert!(status.success());
}

/// The same hazard on the captured path: a transcript that cannot be written
/// must not take the drain down with it.
///
/// `/dev/full` fails every write with `ENOSPC`, so the transcript genuinely
/// fails rather than merely looking like it might -- unlinking a file would
/// not have done it, since the descriptor it already holds keeps working.
///
/// The stream is a `duplex` whose buffer is far smaller than what the writer
/// sends, so the writer can only run to completion if something kept reading.
/// That completion is the assertion, and it is what the old implementation
/// fails: a drain that gives up at the first transcript error abandons the
/// stream with the writer still going.
///
/// What the writer then *sees* differs from production by construction -- a
/// dropped `duplex` half reports an error, where a real pipe whose read end
/// [`Owned::keepalive`] still holds open would block instead. The property
/// under test is the one that matters either way: the drain does not stop
/// before its stream ends.
#[tokio::test(flavor = "current_thread")]
async fn a_failing_transcript_does_not_stop_the_drain() {
    let transcript = Transcript::create(Path::new("/dev/full"), false)
        .await
        .expect("a transcript whose every write fails");

    let (mut writer, reader) = tokio::io::duplex(64);
    let pump = tokio::spawn(async move {
        for _ in 0..200 {
            writer.write_all(b"a line of output\n").await?;
        }
        writer.shutdown().await
    });

    let drained = tokio::time::timeout(
        Duration::from_secs(20),
        super::capture_stream(reader, "test", Some(transcript)),
    )
    .await
    .expect("the drain stopped reading and never returned");

    assert!(
        drained.is_err(),
        "a transcript that failed every write should still be reported"
    );
    tokio::time::timeout(Duration::from_secs(20), pump)
        .await
        .expect("the writer was left blocked on a stream nobody was draining")
        .expect("the pump task panicked")
        .expect("the writer must have been able to finish");
}
