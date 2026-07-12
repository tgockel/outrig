//! Session container watcher.
//!
//! Lifecycle coupling between the primary container and its sidecars is
//! entirely outrig-managed (no pods, no `--requires`, no shared PID
//! namespaces). This module supplies the last coupling layer: a `podman wait`
//! on the primary for the session's lifetime, so a primary that dies out from
//! under outrig (manual `podman kill`, OOM) still reaps its sidecars and ends
//! the session with an error. Companion `podman wait`s per sidecar log
//! sidecar death promptly; nothing restarts.
//!
//! Orderly teardown must call [`SessionWatcher::shutdown`] *before* stopping
//! any container, so its own stops are never mistaken for external death.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{CliError, Result};
use outrig::container::force_remove_detached;

/// Watches a session's containers. Spawned only for sessions that declare
/// sidecars (started or `start = "manual"`) -- a single-container session
/// keeps today's behavior. The sidecar list is shared with the primary
/// reaper so a `/sidecar add` mid-session is covered from the moment it is
/// [registered](SessionWatcher::register_sidecar).
#[derive(Debug)]
pub struct SessionWatcher {
    died: CancellationToken,
    sidecars: Arc<Mutex<Vec<String>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl SessionWatcher {
    /// Arm the watcher: one `podman wait` on the primary (reaps every
    /// registered sidecar and cancels [`SessionWatcher::primary_died`] when
    /// it fires) plus one per sidecar (log-only).
    pub fn spawn(primary: String, sidecars: Vec<String>) -> Self {
        let died = CancellationToken::new();
        let shared: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let primary_task = {
            let died = died.clone();
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                wait_for_container_exit(&primary).await;
                // Read the list when the wait fires, not when the watcher
                // was armed, so dynamically added sidecars are reaped too.
                let names = shared.lock().expect("sidecar name list lock").clone();
                for name in &names {
                    force_remove_detached(name);
                }
                eprintln!(
                    "[outrig] primary container {primary} exited unexpectedly; \
                     reaping {} sidecar container(s)",
                    names.len()
                );
                died.cancel();
            })
        };

        let mut watcher = Self {
            died,
            sidecars: shared,
            tasks: vec![primary_task],
        };
        for name in sidecars {
            watcher.register_sidecar(name);
        }
        watcher
    }

    /// Cover one more sidecar container: reaped when the primary dies, plus
    /// a log-only `podman wait` announcing its own death. Called at arm time
    /// for session-start sidecars and mid-session by `/sidecar add`.
    pub fn register_sidecar(&mut self, name: String) {
        self.sidecars
            .lock()
            .expect("sidecar name list lock")
            .push(name.clone());
        self.tasks.push(tokio::spawn(async move {
            wait_for_container_exit(&name).await;
            eprintln!(
                "[outrig] sidecar container {name} exited; \
                 its MCP tools will return errors until the session ends"
            );
            tracing::warn!(
                target: "outrig::cli::watcher",
                "sidecar container {name} exited mid-session"
            );
        }));
    }

    /// Token cancelled when the primary dies externally. Clone into a
    /// `tokio::select!` next to the session's main future.
    pub fn primary_died(&self) -> CancellationToken {
        self.died.clone()
    }

    /// Disarm before orderly teardown. Aborting the tasks drops their
    /// `podman wait` children (spawned `kill_on_drop`), so no watcher fires
    /// for stops outrig performs itself.
    pub fn shutdown(self) {
        for task in self.tasks {
            task.abort();
        }
    }
}

/// The session-ending error for a watcher-reported external primary death.
/// Every constructor of this condition funnels here so the typed
/// [`CliError::SessionMonitorStopped`] variant stays the single signal.
pub fn primary_death_error(primary: &str) -> CliError {
    CliError::SessionMonitorStopped(format!(
        "primary container {primary} exited unexpectedly; session ended"
    ))
}

/// After teardown: when the session ended because a monitored container went
/// away, print the error and exit instead of returning. Tokio's blocking
/// stdin read (MCP stdio transport, REPL) never returns while the peer holds
/// the pipe open, so a graceful runtime shutdown would hang.
pub fn exit_if_monitor_stopped(outcome: &Result<i32>, final_exit: i32) {
    if let Err(e @ CliError::SessionMonitorStopped(_)) = outcome {
        eprintln!("error: {e}");
        std::process::exit(final_exit.clamp(0, 255));
    }
}

/// Block until the named container exits. `podman wait` returning an error
/// (e.g. no such container) is treated as "already gone".
pub(crate) async fn wait_for_container_exit(name: &str) {
    let child = tokio::process::Command::new("podman")
        .args(["wait", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    match child {
        Ok(mut child) => {
            let _ = child.wait().await;
        }
        Err(e) => {
            tracing::warn!(
                target: "outrig::cli::watcher",
                "podman wait {name} failed to spawn: {e}"
            );
        }
    }
}
