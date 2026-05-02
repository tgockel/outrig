//! Generic subprocess wrappers used by image/container modules. Provides
//! consistent stderr capture, structured failure errors, and tracing-friendly
//! streamed output. Knows nothing about buildah or podman -- callers pass the
//! program name in.

use std::ffi::{OsStr, OsString};
use std::process::{ExitStatus, Output, Stdio};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::error::{OutrigError, Result};

const STDERR_TAIL_LIMIT: usize = 2 * 1024;
const TRUNCATED_MARKER: &str = "... (truncated) ...\n";

#[derive(Debug, Clone)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<OsString>,
}

impl Cmd {
    pub fn new(program: &'static str) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
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
    pub fn to_tokio_command(&self) -> Command {
        let mut c = Command::new(self.program);
        c.args(&self.args);
        c
    }
}

/// Spawn the command, capture stdout and stderr, and return the `Output`
/// regardless of exit status. Only true I/O / spawn failures propagate as
/// errors. Use this when a non-zero exit is meaningful information to the
/// caller (e.g. `git rev-parse --git-dir` for "is this a git repo?",
/// `buildah images --quiet TAG` for "does this tag exist?") rather than an
/// error condition.
pub async fn try_capture(cmd: Cmd) -> Result<Output> {
    Ok(cmd.to_tokio_command().output().await?)
}

/// Spawn the command, capture stdout and stderr, and return the `Output` on
/// success. On non-zero (or signal) exit, return [`OutrigError::Process`] with
/// the program, argv, exit code, and the last `STDERR_TAIL_LIMIT` bytes of
/// stderr (lossy UTF-8, prefixed with a truncation marker if elision occurred).
pub async fn run_capture(cmd: Cmd) -> Result<Output> {
    let output = try_capture(cmd.clone()).await?;
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
pub async fn run_streamed(cmd: Cmd, prefix: &'static str) -> Result<ExitStatus> {
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
pub async fn spawn_stdio(cmd: Cmd) -> Result<Child> {
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
