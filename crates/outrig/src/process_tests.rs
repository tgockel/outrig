//! Unit tests for `process`: covers all three call patterns
//! (`run_capture`, `run_streamed`, `spawn_stdio`), the structured `Process`
//! error variant, and the honest stderr-tail truncation behavior.

use std::ffi::OsString;
use std::io;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::fmt::MakeWriter;

use crate::error::OutrigError;

use super::{Cmd, Transcript};

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
