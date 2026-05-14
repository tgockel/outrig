//! Integration tests for `outrig::repl::Repl::run_with`.
//!
//! These exercise the I/O loop through `tokio::io::duplex` halves, with a
//! `tokio::sync::Notify`-driven interrupt source standing in for SIGINT.
//! The production `Repl::run` is just a thin wrapper that hands real
//! stdin/stdout/stderr and `tokio::signal::ctrl_c()` to `run_with`.

use std::future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, duplex};
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;

use outrig::error::Result as OutrigResult;
use outrig::repl::Repl;

const BUF: usize = 4096;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn never_interrupt() -> impl FnMut() -> std::future::Pending<()> {
    || future::pending::<()>()
}

fn noop_tools() -> impl FnMut() -> std::future::Ready<String> {
    || future::ready("[outrig] (no tools registered)\n".to_string())
}

fn noop_reset() -> impl FnMut() -> std::future::Ready<String> {
    || future::ready("[outrig] (no history to reset)\n".to_string())
}

#[tokio::test]
async fn processes_multiple_lines_in_order() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    stdin_w.write_all(b"hello\nworld\n").await.unwrap();
    drop(stdin_w);

    let on_prompt = |s: String| async move { OutrigResult::Ok(format!("got:{s}")) };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "BANNER",
        on_prompt,
        noop_tools(),
        noop_reset(),
    );

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let read_out = stdout_r.read_to_end(&mut stdout_buf);
    let read_err = stderr_r.read_to_end(&mut stderr_buf);

    let (run_res, _, _) = timeout(TEST_TIMEOUT, async {
        tokio::join!(run, read_out, read_err)
    })
    .await
    .expect("test must not hang");
    run_res.expect("run_with must succeed");

    assert_eq!(stdout_buf, b"got:hello\ngot:world\n");

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(stderr.starts_with("BANNER\n"), "stderr was: {stderr:?}");
    assert!(stderr.contains("> "), "no prompt in stderr: {stderr:?}");
}

#[tokio::test]
async fn eof_exits_cleanly() {
    let (stdin_w, stdin_r) = duplex(BUF);
    drop(stdin_w);
    let (stdout_w, _stdout_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = calls.clone();
    let on_prompt = move |s: String| {
        let c = calls_cb.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            OutrigResult::Ok(s)
        }
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        on_prompt,
        noop_tools(),
        noop_reset(),
    );

    timeout(TEST_TIMEOUT, run)
        .await
        .expect("test must not hang")
        .expect("run_with must succeed");

    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn slash_quit_exits() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"/quit\n").await.unwrap();
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        on_prompt,
        noop_tools(),
        noop_reset(),
    );

    let read_out = async {
        let mut buf = Vec::new();
        stdout_r.read_to_end(&mut buf).await.unwrap();
        buf
    };

    let (run_res, stdout_buf) = timeout(TEST_TIMEOUT, async { tokio::join!(run, read_out) })
        .await
        .expect("test must not hang");
    run_res.expect("run_with must succeed");

    assert!(
        stdout_buf.is_empty(),
        "slash command output must not reach stdout, got: {:?}",
        String::from_utf8_lossy(&stdout_buf)
    );

    drop(stdin_w);
}

#[tokio::test]
async fn empty_line_is_ignored() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"\nhello\n").await.unwrap();
    drop(stdin_w);
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = calls.clone();
    let on_prompt = move |s: String| {
        let c = calls_cb.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            OutrigResult::Ok(format!("got:{s}"))
        }
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        on_prompt,
        noop_tools(),
        noop_reset(),
    );

    let read_out = async {
        let mut buf = Vec::new();
        stdout_r.read_to_end(&mut buf).await.unwrap();
        buf
    };

    let (run_res, stdout_buf) = timeout(TEST_TIMEOUT, async { tokio::join!(run, read_out) })
        .await
        .expect("test must not hang");
    run_res.expect("run_with must succeed");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(stdout_buf, b"got:hello\n");
}

#[tokio::test]
async fn empty_prompt_reply_produces_no_stdout() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"hello\n").await.unwrap();
    drop(stdin_w);
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        on_prompt,
        noop_tools(),
        noop_reset(),
    );

    let read_out = async {
        let mut buf = Vec::new();
        stdout_r.read_to_end(&mut buf).await.unwrap();
        buf
    };

    let (run_res, stdout_buf) = timeout(TEST_TIMEOUT, async { tokio::join!(run, read_out) })
        .await
        .expect("test must not hang");
    run_res.expect("run_with must succeed");

    assert!(
        stdout_buf.is_empty(),
        "empty prompt reply must not write a newline, got: {:?}",
        String::from_utf8_lossy(&stdout_buf)
    );
}

#[tokio::test]
async fn sigint_mid_callback_returns_to_prompt() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"slow\n").await.unwrap();
    let (stdout_w, _stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    let notify = Arc::new(Notify::new());
    let notify_cb = notify.clone();
    let interrupt = move || {
        let n = notify_cb.clone();
        async move { n.notified().await }
    };

    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started_tx_cell: std::sync::Mutex<Option<oneshot::Sender<()>>> =
        std::sync::Mutex::new(Some(started_tx));
    let on_prompt = move |_: String| {
        let tx = started_tx_cell.lock().unwrap().take();
        async move {
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
            future::pending::<OutrigResult<String>>().await
        }
    };

    let run_handle = tokio::spawn(async move {
        Repl::run_with(
            BufReader::new(stdin_r),
            stdout_w,
            stderr_w,
            interrupt,
            "",
            on_prompt,
            noop_tools(),
            noop_reset(),
        )
        .await
    });

    started_rx.await.expect("on_prompt must signal start");
    notify.notify_one();

    let mut stderr_buf = Vec::new();
    let drain = read_until_contains(&mut stderr_r, &mut stderr_buf, "interrupted");
    timeout(TEST_TIMEOUT, drain)
        .await
        .expect("must observe interrupted notice within timeout");

    drop(stdin_w);

    timeout(TEST_TIMEOUT, run_handle)
        .await
        .expect("run_with must finish")
        .expect("spawn join")
        .expect("run_with must succeed");

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] interrupted"),
        "stderr lacked interrupt notice: {stderr:?}"
    );
}

#[tokio::test]
async fn slash_tools_and_reset_invoke_callbacks() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"/tools\n/reset\n").await.unwrap();
    drop(stdin_w);
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    let tools_calls = Arc::new(AtomicUsize::new(0));
    let reset_calls = Arc::new(AtomicUsize::new(0));
    let tools_cb = tools_calls.clone();
    let reset_cb = reset_calls.clone();

    let on_tools = move || {
        let c = tools_cb.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            "[outrig] tools available (1):\n  fs__list_directory   List a directory.\n".to_string()
        }
    };
    let on_reset = move || {
        let c = reset_cb.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            "[outrig] history cleared".to_string() // no trailing newline; REPL adds one
        }
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        on_prompt,
        on_tools,
        on_reset,
    );

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let read_out = stdout_r.read_to_end(&mut stdout_buf);
    let read_err = stderr_r.read_to_end(&mut stderr_buf);

    let (run_res, _, _) = timeout(TEST_TIMEOUT, async {
        tokio::join!(run, read_out, read_err)
    })
    .await
    .expect("test must not hang");
    run_res.expect("run_with must succeed");

    assert_eq!(tools_calls.load(Ordering::SeqCst), 1);
    assert_eq!(reset_calls.load(Ordering::SeqCst), 1);
    assert!(
        stdout_buf.is_empty(),
        "slash output must not reach stdout, got: {:?}",
        String::from_utf8_lossy(&stdout_buf)
    );

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] tools available (1):"),
        "stderr lacked /tools text: {stderr:?}"
    );
    assert!(
        stderr.contains("[outrig] history cleared\n"),
        "stderr lacked /reset text (with REPL-appended newline): {stderr:?}"
    );
}

async fn read_until_contains(stream: &mut DuplexStream, sink: &mut Vec<u8>, needle: &str) {
    let mut chunk = [0u8; 256];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .expect("read from stderr duplex");
        if n == 0 {
            break;
        }
        sink.extend_from_slice(&chunk[..n]);
        if std::str::from_utf8(sink)
            .map(|s| s.contains(needle))
            .unwrap_or(false)
        {
            break;
        }
    }
}
