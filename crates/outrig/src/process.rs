//! Process transcript support.
//!
//! The public piece is [`Transcript`], which mirrors command lines and output
//! into a log file for runtime startup paths. The generic command/capture
//! helpers in this module are crate-private implementation details used by
//! the container and image runtimes.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::time::Instant;

use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::error::{OutrigError, Result};

const STDERR_TAIL_LIMIT: usize = 1024 * 1024;
const TRUNCATED_MARKER: &str = "... (truncated) ...\n";
const STREAM_READ_CHUNK: usize = 8 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Cmd {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<OsString>,
}

impl Cmd {
    pub(crate) fn new(program: &'static str) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub(crate) fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub(crate) fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|s| s.as_ref().to_os_string()));
        self
    }

    /// Build a fresh `tokio::process::Command` from this argv. No stdio
    /// configuration is applied -- the caller layers `.stdin()` / `.stdout()`
    /// / `.stderr()` to taste before spawning.
    pub(crate) fn to_tokio_command(&self) -> Command {
        let mut c = Command::new(self.program);
        c.args(&self.args);
        c
    }

    /// Render the argv as a shell-like command line for diagnostics. This is
    /// display-only; callers must still spawn via `Command` so no quoting
    /// participates in execution.
    pub(crate) fn render(&self) -> String {
        std::iter::once(OsStr::new(self.program))
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(render_arg)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Clone, Debug)]
pub struct Transcript {
    file: Arc<Mutex<tokio::fs::File>>,
    stderr: bool,
}

impl Transcript {
    /// Create a new transcript file, truncating any stale content at `path`.
    /// When `stderr` is true, every transcript line is also mirrored to the
    /// process's stderr.
    pub async fn create(path: &Path, stderr: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            stderr,
        })
    }

    /// Record one already-rendered logical line with the conventional
    /// `[prefix]` marker.
    pub async fn line(&self, prefix: &'static str, line: &str) -> std::io::Result<()> {
        let rendered = format!("[{prefix}] {line}\n");
        self.write_all(rendered.as_bytes()).await
    }

    async fn write_all(&self, bytes: &[u8]) -> std::io::Result<()> {
        if self.stderr {
            let mut stderr = tokio::io::stderr();
            stderr.write_all(bytes).await?;
            stderr.flush().await?;
        }
        let mut file = self.file.lock().await;
        file.write_all(bytes).await?;
        file.flush().await
    }
}

/// Spawn the command, capture stdout and stderr, and return the `Output`
/// regardless of exit status. Only true I/O / spawn failures propagate as
/// errors. Use this when a non-zero exit is meaningful information to the
/// caller (e.g. `git rev-parse --git-dir` for "is this a git repo?",
/// `buildah images --quiet TAG` for "does this tag exist?") rather than an
/// error condition.
pub(crate) async fn try_capture(cmd: Cmd) -> Result<Output> {
    Ok(cmd.to_tokio_command().output().await?)
}

/// Spawn the command, capture stdout and stderr, and optionally tee a
/// transcript of the command line plus both output streams. Non-zero exit is
/// returned in the `Output`, matching [`try_capture`].
pub(crate) async fn try_capture_logged(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
) -> Result<Output> {
    let transcript = transcript.cloned();
    if let Some(t) = &transcript {
        t.line(prefix, &format!("$ {}", cmd.render())).await?;
    }

    // The `-v` transcript is opt-in and file-backed; this pair is what makes a
    // stuck `podman run` visible under `RUST_LOG=debug` alone.
    tracing::debug!(target: "outrig::process", command = %cmd.render(), "spawn");
    let started = Instant::now();

    let mut child = cmd
        .to_tokio_command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as piped above");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as piped above");

    let stdout_task = tokio::spawn(capture_stream(stdout, prefix, transcript.clone()));
    let stderr_task = tokio::spawn(capture_stream(stderr, prefix, transcript));

    let status = child.wait().await?;
    tracing::debug!(
        target: "outrig::process",
        program = cmd.program,
        code = ?status.code(),
        elapsed_ms = started.elapsed().as_millis(),
        "exit"
    );
    let stdout = stdout_task.await.expect("stdout capture task panicked")?;
    let stderr = stderr_task.await.expect("stderr capture task panicked")?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Spawn the command, capture stdout and stderr, and return the `Output` on
/// success. On non-zero (or signal) exit, return [`OutrigError::Process`] with
/// the program, argv, exit code, and the last `STDERR_TAIL_LIMIT` bytes of
/// stderr (lossy UTF-8, prefixed with a truncation marker if elision occurred).
pub(crate) async fn run_capture(cmd: Cmd) -> Result<Output> {
    tracing::debug!(target: "outrig::process", command = %cmd.render(), "spawn");
    let started = Instant::now();

    let mut child = cmd
        .to_tokio_command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .expect("stdout was configured as piped above");
    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as piped above");

    let stdout_task = tokio::spawn(capture_all(stdout));
    let stderr_task = tokio::spawn(capture_stderr_tail(stderr));

    let status = child.wait().await?;
    tracing::debug!(
        target: "outrig::process",
        program = cmd.program,
        code = ?status.code(),
        elapsed_ms = started.elapsed().as_millis(),
        "exit"
    );
    let stdout = stdout_task.await.expect("stdout capture task panicked")?;
    let stderr_tail = stderr_task.await.expect("stderr capture task panicked")?;

    if status.success() {
        Ok(Output {
            status,
            stdout,
            stderr: stderr_tail.into_bytes(),
        })
    } else {
        Err(OutrigError::Process {
            program: cmd.program,
            argv: cmd.args,
            exit_code: status.code(),
            stderr_tail: stderr_tail.into_tail_string(),
        })
    }
}

/// Logged sibling of [`run_capture`]. On success, returns captured output;
/// on non-zero, returns the same structured process error with a stderr tail.
pub(crate) async fn run_capture_logged(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
) -> Result<Output> {
    let output = try_capture_logged(cmd.clone(), prefix, transcript).await?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(OutrigError::Process {
            program: cmd.program,
            argv: cmd.args,
            exit_code: output.status.code(),
            stderr_tail: tail_string(&output.stderr, STDERR_TAIL_LIMIT),
        })
    }
}

/// Spawn the command with stderr piped, forwarding each stderr line to
/// `tracing::info!` as `[<prefix>] <line>` (target `outrig::process`). stdout
/// inherits the parent's; stdin is null. Returns the [`ExitStatus`] -- a
/// non-zero exit is **not** an error, since callers may want to inspect
/// status before deciding what it means.
pub(crate) async fn run_streamed(cmd: Cmd, prefix: &'static str) -> Result<ExitStatus> {
    let mut child = cmd
        .to_tokio_command()
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()?;

    let stderr = child
        .stderr
        .take()
        .expect("stderr was configured as piped above");

    let log_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "outrig::process", "[{prefix}] {line}");
        }
    });

    let status = child.wait().await?;
    let _ = log_task.await;
    Ok(status)
}

/// Spawn the command with all three of stdin/stdout/stderr piped, returning
/// the [`Child`]. The caller owns the child and is responsible for waiting on
/// it. Used by `podman exec -i` callers that want full bidirectional control.
pub(crate) async fn spawn_stdio(cmd: Cmd) -> Result<Child> {
    let child = cmd
        .to_tokio_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    Ok(child)
}

fn tail_string(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        let start = bytes.len() - limit;
        let mut out = String::with_capacity(limit + TRUNCATED_MARKER.len());
        out.push_str(TRUNCATED_MARKER);
        out.push_str(&String::from_utf8_lossy(&bytes[start..]));
        out
    }
}

async fn capture_all<R>(stream: R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut captured = Vec::new();
    reader.read_to_end(&mut captured).await?;
    Ok(captured)
}

async fn capture_stderr_tail<R>(stream: R) -> std::io::Result<BoundedStderrTail>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut captured = BoundedStderrTail::new();
    let mut chunk = [0_u8; STREAM_READ_CHUNK];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        captured.push(&chunk[..n]);
    }
    Ok(captured)
}

#[derive(Debug)]
struct BoundedStderrTail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

impl BoundedStderrTail {
    fn new() -> Self {
        Self {
            bytes: VecDeque::with_capacity(STDERR_TAIL_LIMIT),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() > STDERR_TAIL_LIMIT {
            self.bytes.clear();
            self.bytes
                .extend(chunk[chunk.len() - STDERR_TAIL_LIMIT..].iter().copied());
            self.truncated = true;
            return;
        }

        let overflow = self.bytes.len() + chunk.len();
        if overflow > STDERR_TAIL_LIMIT {
            self.bytes.drain(..overflow - STDERR_TAIL_LIMIT);
            self.truncated = true;
        }
        self.bytes.extend(chunk.iter().copied());
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_iter().collect()
    }

    fn into_tail_string(self) -> String {
        let truncated = self.truncated;
        let bytes = self.into_bytes();
        if truncated {
            let mut out = String::with_capacity(bytes.len() + TRUNCATED_MARKER.len());
            out.push_str(TRUNCATED_MARKER);
            out.push_str(&String::from_utf8_lossy(&bytes));
            out
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

async fn capture_stream<R>(
    stream: R,
    prefix: &'static str,
    transcript: Option<Transcript>,
) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    let mut captured = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            break;
        }
        captured.extend_from_slice(&line);
        if let Some(t) = &transcript {
            let rendered = String::from_utf8_lossy(&line);
            t.line(prefix, rendered.trim_end_matches(['\r', '\n']))
                .await?;
        }
    }
    Ok(captured)
}

fn render_arg(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(is_shell_safe_byte) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn is_shell_safe_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'/' | b'.' | b'-' | b'_' | b':' | b'=' | b',' | b'+' | b'@' | b'%'
        )
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod process_tests;
