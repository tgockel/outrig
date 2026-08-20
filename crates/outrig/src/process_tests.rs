//! Unit tests for `process`: covers all four call patterns (`run_capture`,
//! `try_capture_logged`, `run_streamed`, `spawn_stdio`), the structured
//! `Process` error variant, the spawn/exit tracing every podman invocation
//! relies on, the honest stderr-tail truncation behavior, and both halves of
//! the ownership guarantee -- the bound a dropped future gets and the
//! confirmed reap a stop signal gets.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::MakeWriter;

use crate::error::OutrigError;

use super::{Cmd, Transcript};

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
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");
    let status = rt.block_on(async {
        super::run_streamed(
            Cmd::new("/bin/sh").args(["-c", "echo hello-from-stderr 1>&2"]),
            "test",
        )
        .await
        .expect("run_streamed must succeed")
    });
    assert!(status.success());

    let captured = String::from_utf8(buf.lock().unwrap().clone())
        .expect("captured tracing output must be UTF-8");
    assert!(
        captured.contains("[test] hello-from-stderr"),
        "tracing should receive prefixed stderr line, got: {captured}"
    );
}

#[test]
fn try_capture_logged_traces_spawn_and_exit_at_debug() {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter(buf.clone());
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");
    rt.block_on(async {
        super::try_capture_logged(Cmd::new("/bin/echo").arg("hi"), "test", None)
            .await
            .expect("try_capture_logged must succeed")
    });

    let captured = String::from_utf8(buf.lock().unwrap().clone())
        .expect("captured tracing output must be UTF-8");
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

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

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
        super::try_capture_logged_until(cmd, "test", None, cancel.clone().cancelled_owned()),
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
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = super::try_capture_logged_until(
        pid_publishing_sleeper(&path),
        "test",
        None,
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
    let dir = tempfile::tempdir().expect("tempdir");
    let path = pid_file(&dir);

    let cancel = CancellationToken::new();
    let (result, pid) = tokio::join!(
        super::try_capture_logged_until(
            pid_publishing_sleeper(&path),
            "test",
            None,
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
