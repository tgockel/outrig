//! End-to-end: a signal ends `outrig run` and `outrig mcp` through teardown.
//! Gated behind `--features e2e`.
//!
//! Each case starts the binary against the mcp-fs fixture with its stdin held
//! open, as a terminal or an MCP client would, signals it at a chosen point,
//! and checks the three things a signal used to leave behind (#327): the
//! process exits with the signal's code, `session.json` is finalized with that
//! code, and the session's container is gone. Before the fix the container ran
//! on under conmon, and `outrig discard` refused the record as still running.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p outrig-cli --features e2e --test signal_teardown -- --nocapture
//! ```

#![cfg(feature = "e2e")]

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use outrig_cli::session::{Session, SessionStore};
use rmcp::service::serve_client;
use tempfile::TempDir;
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;

mod common;
use common::{
    E2E_TIMEOUT, GLOBAL_WITH_MODEL, fixture_mcp_fs_dir, podman_names, stream_lines,
    wait_for_stderr_value, wait_until_gone,
};

/// The filesystem server the fixture image carries.
const FS_SERVER: &str = r#"["mcp-server-filesystem", "/workspace"]"#;

/// `cat` echoes `initialize` back as if it were the server's own request and
/// never answers it, so startup waits on it indefinitely.
const SILENT_SERVER: &str = r#"["cat"]"#;

/// A fixture repo whose image hosts one MCP server, `fs`, run as `fs_server`,
/// and `extra` more, appended to the same table.
/// The model `outrig run` needs comes from [`GLOBAL_WITH_MODEL`], passed as
/// the global config; `outrig mcp` ignores it.
fn write_config(repo: &Path, fs_server: &str, extra: &str) {
    let agents_dir = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents_dir).expect("mkdir .agents/outrig");
    let config_toml = format!(
        r#"
default-image = "smoke"

[images.smoke]
dockerfile = "{dockerfile}"
context = "{context}"

  [images.smoke.mcp]
  fs = {fs_server}
{extra}
"#,
        dockerfile = fixture_mcp_fs_dir().join("Dockerfile").display(),
        context = fixture_mcp_fs_dir().display(),
    );
    std::fs::write(agents_dir.join("config.toml"), config_toml).expect("write config");
    std::fs::write(repo.join("global.toml"), GLOBAL_WITH_MODEL).expect("write global config");
}

/// One `outrig` process, its stdin held open, and the directories it writes.
struct Outrig {
    child: Child,
    /// Held here, not in `child`: `Child::wait` closes the stdin it still
    /// owns before it waits, and EOF would end the REPL on its own, racing
    /// the signal under test for the exit code.
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    stderr_task: JoinHandle<()>,
    sessions: TempDir,
    session_dir: TempDir,
    _repo: TempDir,
}

impl Outrig {
    /// Start `outrig <command...> --session-dir <dir>` against a fresh fixture
    /// repo. stdin stays with the caller through `stdin`, stdout through
    /// `child`.
    fn spawn(command: &[&str], fs_server: &str) -> Self {
        Self::spawn_with(command, fs_server, "")
    }

    /// [`Outrig::spawn`] with `extra` MCP servers declared beside `fs`.
    fn spawn_with(command: &[&str], fs_server: &str, extra: &str) -> Self {
        let repo = tempfile::tempdir().expect("tempdir repo");
        write_config(repo.path(), fs_server, extra);
        let sessions = tempfile::tempdir().expect("tempdir sessions");
        let session_dir = tempfile::tempdir().expect("tempdir session");

        let mut child = Command::new(env!("CARGO_BIN_EXE_outrig"))
            .arg("--global-config")
            .arg(repo.path().join("global.toml"))
            .arg("--session-root")
            .arg(sessions.path())
            .args(command)
            .arg("--session-dir")
            .arg(session_dir.path())
            .current_dir(repo.path())
            .env("OUTRIG_TEST_KEY", "test-key")
            .env("OUTRIG_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn outrig");
        let stdin = child.stdin.take();
        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_task = tokio::spawn(stream_lines(
            child.stderr.take().expect("stderr piped"),
            stderr.clone(),
            "stderr",
        ));
        Self {
            child,
            stdin,
            stderr,
            stderr_task,
            sessions,
            session_dir,
            _repo: repo,
        }
    }

    async fn wait_for_line(&self, prefix: &str) {
        wait_for_stderr_value(self.stderr.clone(), prefix).await;
    }

    fn signal(&self, signal: Signal) {
        let pid = self.child.id().expect("outrig is running");
        kill(Pid::from_raw(pid as i32), signal).expect("signal outrig");
    }

    /// Wait for the exit, with stdin still open: a blocking stdin read that
    /// held the process up after teardown is one of the ways this can fail.
    async fn exit(&mut self) -> (ExitStatus, String) {
        let status = timeout(E2E_TIMEOUT, self.child.wait())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "outrig did not exit within {E2E_TIMEOUT:?}; stderr: {}",
                    self.stderr.lock().unwrap()
                )
            })
            .expect("wait for outrig");
        self.stdin.take();
        let _ = (&mut self.stderr_task).await;
        let stderr = self.stderr.lock().unwrap().clone();
        (status, stderr)
    }

    /// The session's record, finalized with `exit_code`.
    fn assert_finalized(&self, exit_code: i32) -> Session {
        let record = SessionStore::new(self.sessions.path().to_path_buf())
            .get_by_path(self.session_dir.path())
            .expect("read the session record");
        assert!(
            record.ended_at.is_some(),
            "record not finalized: {record:?}"
        );
        assert_eq!(record.exit_code, Some(exit_code), "record: {record:?}");
        record
    }

    /// The record is finalized with `exit_code`, and its container is gone.
    async fn assert_torn_down(&self, exit_code: i32) -> Session {
        let record = self.assert_finalized(exit_code);
        let left = podman_names(&format!("name={}", record.container_name)).await;
        assert!(left.is_empty(), "container still exists: {left:?}");
        record
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_sigterm_at_the_prompt_tears_the_session_down() {
    common::init_tracing();
    let mut outrig = Outrig::spawn(&["run"], FS_SERVER);
    outrig.wait_for_line("[outrig] entering REPL").await;

    outrig.signal(Signal::SIGTERM);
    let (status, stderr) = outrig.exit().await;

    assert_eq!(status.code(), Some(143), "stderr: {stderr}");
    assert!(
        stderr.contains("[outrig] SIGTERM received; ending the session"),
        "stderr: {stderr}"
    );
    let record = outrig.assert_torn_down(143).await;

    // The symptom users hit: discard used to refuse the record as running.
    let discard = Command::new(env!("CARGO_BIN_EXE_outrig"))
        .arg("--session-root")
        .arg(outrig.sessions.path())
        .args(["discard", record.id.as_str(), "--yes"])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run outrig discard");
    assert!(
        discard.status.success(),
        "discard refused the session: {}",
        String::from_utf8_lossy(&discard.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_sigint_during_an_unanswered_initialize_tears_the_session_down() {
    common::init_tracing();
    let mut outrig = Outrig::spawn(&["run"], SILENT_SERVER);
    outrig.wait_for_line("[outrig] MCP fs: initializing").await;

    outrig.signal(Signal::SIGINT);
    let (status, stderr) = outrig.exit().await;

    assert_eq!(status.code(), Some(130), "stderr: {stderr}");
    outrig.assert_torn_down(130).await;
}

/// The second signal cuts teardown short. Which path the cleanup took depends
/// on how far teardown had got, so this asserts only what both must leave.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_second_sigterm_during_teardown_still_leaves_nothing() {
    common::init_tracing();
    let mut outrig = Outrig::spawn(&["run"], FS_SERVER);
    outrig.wait_for_line("[outrig] entering REPL").await;

    outrig.signal(Signal::SIGTERM);
    outrig
        .wait_for_line("[outrig] SIGTERM received; ending the session")
        .await;
    outrig.signal(Signal::SIGTERM);
    let (status, stderr) = outrig.exit().await;

    assert_eq!(status.code(), Some(143), "stderr: {stderr}");
    let record = outrig.assert_finalized(143);
    // A cut-short teardown hands the container to a detached removal.
    wait_until_gone(&[record.container_name]).await;
}

/// A signal is how a server is stopped, so serving keeps exiting `0` -- and
/// the client holding stdin open must not hold the exit up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_sigterm_while_serving_over_stdio_exits_zero() {
    common::init_tracing();
    let mut outrig = Outrig::spawn(&["mcp"], FS_SERVER);
    let stdin = outrig.stdin.take().expect("stdin piped");
    let stdout = outrig.child.stdout.take().expect("stdout piped");
    let client = timeout(E2E_TIMEOUT, serve_client((), (stdout, stdin)))
        .await
        .expect("handshake within timeout")
        .expect("client handshake");
    outrig.wait_for_line("[outrig] mcp server ready").await;

    outrig.signal(Signal::SIGTERM);
    let (status, stderr) = outrig.exit().await;
    drop(client);

    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    outrig.assert_torn_down(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_sighup_during_an_unanswered_initialize_tears_the_session_down() {
    common::init_tracing();
    let mut outrig = Outrig::spawn(&["mcp"], SILENT_SERVER);
    outrig.wait_for_line("[outrig] MCP fs: initializing").await;

    outrig.signal(Signal::SIGHUP);
    let (status, stderr) = outrig.exit().await;

    assert_eq!(status.code(), Some(129), "stderr: {stderr}");
    outrig.assert_torn_down(129).await;
}

/// `show-merged` writes a table larger than a pipe holds to a stdout nobody
/// reads, so the write blocks. A signal has to end the session around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_show_merged_blocked_on_stdout_ends_on_sigterm() {
    common::init_tracing();
    // Many short entries rather than a few long ones: the table is also
    // baked into an image label, which buildah takes as one argument, and the
    // kernel caps one argument at 128 KiB. Rendered with a comment line per
    // server, two thousand of them still run past a 64 KiB pipe.
    let extra: String = (0..2000)
        .map(|i| format!("  pad{i} = [\"cat\"]\n"))
        .collect();
    let mut outrig = Outrig::spawn_with(&["mcp", "show-merged"], FS_SERVER, &extra);
    outrig.wait_for_line("[outrig] container user ready").await;
    // Long enough for the table to fill the pipe and the write to stall.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    outrig.signal(Signal::SIGTERM);
    let (status, stderr) = outrig.exit().await;

    assert_eq!(status.code(), Some(143), "stderr: {stderr}");
    outrig.assert_torn_down(143).await;
}
