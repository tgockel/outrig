//! Integration tests for `outrig::process`: covers all three call patterns
//! (`run_capture`, `run_streamed`, `spawn_stdio`), the structured `Process`
//! error variant, and the honest stderr-tail truncation behavior.

use std::ffi::OsString;
use std::io;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::fmt::MakeWriter;

use outrig::error::OutrigError;
use outrig::process::{self, Cmd};

#[tokio::test(flavor = "current_thread")]
async fn run_capture_echo_succeeds() {
    let out = process::run_capture(Cmd::new("/bin/echo").arg("hi"))
        .await
        .expect("/bin/echo hi must succeed");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"hi\n");
}

#[tokio::test(flavor = "current_thread")]
async fn run_capture_false_fails_with_exit_1() {
    let err = process::run_capture(Cmd::new("/bin/false").arg("ignored-arg"))
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
async fn run_capture_truncates_long_stderr_with_marker() {
    // Emit 5000 lines to stderr; each line is short, total exceeds 2 KiB.
    let cmd = Cmd::new("/bin/sh").args([
        "-c",
        "for i in $(seq 1 5000); do echo line-$i 1>&2; done; exit 1",
    ]);
    let err = process::run_capture(cmd)
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
        process::run_streamed(
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
    let mut child = process::spawn_stdio(Cmd::new("/bin/cat"))
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
