//! Integration tests for `outrig_cli::repl::Repl::run_with`.
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

use outrig_cli::error::Result as OutrigResult;
use outrig_cli::repl::{HelpEntry, Repl};

const BUF: usize = 4096;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Every `(name, args)` pair a test dispatcher received, in order.
type SeenCommands = Arc<std::sync::Mutex<Vec<(String, Vec<String>)>>>;

fn never_interrupt() -> impl FnMut() -> std::future::Pending<()> {
    || future::pending::<()>()
}

/// Dispatcher that knows no commands: everything is reported unknown.
fn no_commands() -> impl FnMut(String, Vec<String>) -> std::future::Ready<Option<String>> {
    |_, _| future::ready(None)
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
        &[],
        on_prompt,
        no_commands(),
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
        &[],
        on_prompt,
        no_commands(),
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
        &[],
        on_prompt,
        no_commands(),
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
        &[],
        on_prompt,
        no_commands(),
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
        &[],
        on_prompt,
        no_commands(),
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
            &[],
            on_prompt,
            no_commands(),
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

/// One dispatcher serves every caller command; it receives the command name
/// plus whitespace-split args and its `Some(..)` text lands on stderr with a
/// REPL-appended newline.
#[tokio::test]
async fn dispatcher_routes_commands_with_split_args() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w
        .write_all(b"/tools\n/reset\n/sidecar add tools\n/sidecar list\n/sidecar\n")
        .await
        .unwrap();
    drop(stdin_w);
    let (stdout_w, mut stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    let seen: SeenCommands = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_cb = seen.clone();
    let on_command = move |cmd: String, args: Vec<String>| {
        let seen = seen_cb.clone();
        async move {
            let text = match cmd.as_str() {
                "tools" => "[outrig] tools available (0):\n".to_string(),
                "reset" => "[outrig] history cleared".to_string(), // no trailing newline
                _ => "[outrig] sidecar handled".to_string(),
            };
            seen.lock().unwrap().push((cmd, args));
            Some(text)
        }
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        &[],
        on_prompt,
        on_command,
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

    let owned = |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("tools".to_string(), owned(&[])),
            ("reset".to_string(), owned(&[])),
            ("sidecar".to_string(), owned(&["add", "tools"])),
            ("sidecar".to_string(), owned(&["list"])),
            ("sidecar".to_string(), owned(&[])),
        ]
    );
    assert!(
        stdout_buf.is_empty(),
        "slash output must not reach stdout, got: {:?}",
        String::from_utf8_lossy(&stdout_buf)
    );

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] tools available (0):"),
        "stderr lacked /tools text: {stderr:?}"
    );
    assert!(
        stderr.contains("[outrig] history cleared\n"),
        "stderr lacked /reset text (with REPL-appended newline): {stderr:?}"
    );
    assert!(
        stderr.contains("[outrig] sidecar handled\n"),
        "stderr lacked /sidecar text: {stderr:?}"
    );
}

/// A `None` from the dispatcher produces the unknown-command notice echoing
/// the input as typed -- including arguments and their original whitespace.
#[tokio::test]
async fn unknown_command_prints_raw_text() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w
        .write_all(b"/bogus one  two\n/tools extra\n")
        .await
        .unwrap();
    drop(stdin_w);
    let (stdout_w, _stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    // A `/tools` that rejects arguments by reporting itself unknown, the
    // convention run.rs's dispatcher uses for zero-arg commands.
    let on_command = |cmd: String, args: Vec<String>| async move {
        (cmd == "tools" && args.is_empty()).then(|| "[outrig] tools".to_string())
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        &[],
        on_prompt,
        on_command,
    );

    let mut stderr_buf = Vec::new();
    let read_err = stderr_r.read_to_end(&mut stderr_buf);

    let (run_res, _) = timeout(TEST_TIMEOUT, async { tokio::join!(run, read_err) })
        .await
        .expect("test must not hang");
    run_res.expect("run_with must succeed");

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] unknown command: /bogus one  two\n"),
        "notice must echo raw input: {stderr:?}"
    );
    assert!(
        stderr.contains("[outrig] unknown command: /tools extra\n"),
        "args on a zero-arg command must stay unknown: {stderr:?}"
    );
}

#[tokio::test]
async fn slash_help_composes_caller_entries() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    stdin_w.write_all(b"/help\n").await.unwrap();
    drop(stdin_w);
    let (stdout_w, _stdout_r) = duplex(BUF);
    let (stderr_w, mut stderr_r) = duplex(BUF);

    let on_prompt = |_: String| async move { OutrigResult::Ok(String::new()) };

    const COMMANDS: &[HelpEntry] = &[
        HelpEntry {
            syntax: "/tools",
            description: "list registered tools",
        },
        HelpEntry {
            syntax: "/sidecar add <name>",
            description: "start a sidecar",
        },
    ];

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        COMMANDS,
        on_prompt,
        no_commands(),
    );

    let mut stderr_buf = Vec::new();
    let read_err = stderr_r.read_to_end(&mut stderr_buf);

    let (run_res, _) = timeout(TEST_TIMEOUT, async { tokio::join!(run, read_err) })
        .await
        .expect("test must not hang");
    run_res.expect("run_with must succeed");

    let stderr = String::from_utf8(stderr_buf).expect("stderr utf-8");
    assert!(
        stderr.contains("[outrig] slash commands:"),
        "help lacked header: {stderr:?}"
    );
    // Caller entries sandwiched between the built-in /help and /quit lines,
    // all padded to the widest syntax (19 code points here).
    for line in [
        "  /help                 show this help",
        "  /tools                list registered tools",
        "  /sidecar add <name>   start a sidecar",
        "  /quit                 exit the session",
    ] {
        assert!(stderr.contains(line), "help lacked {line:?}: {stderr:?}");
    }
}

/// A prompt callback that returns `Err` still ends the loop, and no further
/// line is read.
///
/// This is the invariant that survives the transient-failure recovery: a turn
/// killed by a rate limit is now handled *below* `on_prompt` and comes back as
/// `Ok`, but `SessionMonitorStopped` -- the primary container dying -- has to
/// keep escaping, because the caller hard-exits on it rather than prompting
/// into a dead session. Pinned here because a refactor that made the loop
/// forgiving would silently swallow it.
#[tokio::test]
async fn prompt_callback_error_ends_the_loop() {
    let (mut stdin_w, stdin_r) = duplex(BUF);
    let (stdout_w, _stdout_r) = duplex(BUF);
    let (stderr_w, _stderr_r) = duplex(BUF);

    // Two lines: the second must never be read.
    stdin_w.write_all(b"boom\nagain\n").await.unwrap();
    drop(stdin_w);

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_cb = calls.clone();
    let on_prompt = move |_: String| {
        let c = calls_cb.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            OutrigResult::Err(outrig_cli::error::CliError::SessionMonitorStopped(
                "primary died".to_string(),
            ))
        }
    };

    let run = Repl::run_with(
        BufReader::new(stdin_r),
        stdout_w,
        stderr_w,
        never_interrupt(),
        "",
        &[],
        on_prompt,
        no_commands(),
    );

    let err = timeout(TEST_TIMEOUT, run)
        .await
        .expect("test must not hang")
        .expect_err("a fatal prompt error must escape the loop");
    assert!(
        matches!(err, outrig_cli::error::CliError::SessionMonitorStopped(_)),
        "the error must arrive intact, got: {err:?}",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the loop must stop at the first error, not read the next line",
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
