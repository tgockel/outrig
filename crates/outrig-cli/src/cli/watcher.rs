//! Session container watcher.
//!
//! Lifecycle coupling between the primary container and its sidecars is
//! entirely outrig-managed (no pods, no `--requires`, no shared PID
//! namespaces). This module supplies the last coupling layer: a single
//! `podman events` stream, filtered to this session's containers, running for
//! the session's lifetime. A primary that dies out from under outrig (manual
//! `podman kill`, OOM) reaps its sidecars and ends the session with an error;
//! a sidecar death is logged; nothing restarts. One events process replaces
//! the former 1-plus-N `podman wait` children (`podman wait` given several
//! names waits for *all* of them, so it cannot batch a session's containers).
//!
//! Orderly teardown must call [`SessionWatcher::shutdown`] *before* stopping
//! any container, so its own stops are never mistaken for external death.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::{CliError, Result};
use outrig::container::{LABEL_SESSION, force_remove_detached};

/// Watches a session's containers. Spawned only for sessions that declare
/// sidecars (started or `start = "manual"`) -- a single-container session
/// keeps today's behavior. The sidecar list is shared with the events reader
/// so a `/sidecar add` mid-session is covered from the moment it is
/// [registered](SessionWatcher::register_sidecar); the label-filtered stream
/// already delivers the new container's death, so registration only has to
/// record the name for the primary-death reap.
#[derive(Debug)]
pub struct SessionWatcher {
    died: CancellationToken,
    sidecars: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl SessionWatcher {
    /// Arm the watcher: one `podman events` reader for the whole session. It
    /// reaps every registered sidecar and cancels [`SessionWatcher::primary_died`]
    /// when the primary dies, and logs any sidecar death. `since` (the session
    /// start) is replayed so a container that died between start and arm is not
    /// missed.
    pub fn spawn(primary: String, sidecars: Vec<String>, sid: String, since: SystemTime) -> Self {
        let died = CancellationToken::new();
        let sidecars: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(sidecars));
        let task = tokio::spawn(watch_events(
            primary,
            sid,
            since,
            died.clone(),
            Arc::clone(&sidecars),
        ));
        Self {
            died,
            sidecars,
            task,
        }
    }

    /// Cover one more sidecar container: record its name so the primary-death
    /// reap includes it. The session-filtered events stream already reports its
    /// death, so no new process is needed. Called mid-session by `/sidecar add`.
    pub fn register_sidecar(&mut self, name: String) {
        self.sidecars
            .lock()
            .expect("sidecar name list lock")
            .push(name);
    }

    /// Token cancelled when the primary dies externally. Clone into a
    /// `tokio::select!` next to the session's main future.
    pub fn primary_died(&self) -> CancellationToken {
        self.died.clone()
    }

    /// Disarm before orderly teardown. Aborting the task drops its `podman
    /// events` child (spawned `kill_on_drop`), so no watcher fires for stops
    /// outrig performs itself.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

/// Where a `died` event should be routed. Pure classification; the membership
/// and reaping *policy* stays in [`watch_events`] so this stays testable.
#[derive(Debug, PartialEq, Eq)]
enum EventRoute {
    PrimaryDied,
    SidecarDied(String),
    Ignore,
}

/// The subset of a `podman events --format json` object we read. Unknown fields
/// (exit code, id, image, time, attributes) are ignored.
#[derive(Deserialize)]
struct PodmanEvent {
    #[serde(rename = "Type")]
    event_type: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Name")]
    name: String,
}

/// Classify one NDJSON event line. Defensive on `Type`/`Status` even though the
/// command already filters `event=died`; a malformed or unrelated line is
/// [`EventRoute::Ignore`].
fn route_event(line: &str, primary: &str) -> EventRoute {
    let Ok(event) = serde_json::from_str::<PodmanEvent>(line) else {
        return EventRoute::Ignore;
    };
    if event.event_type != "container" || event.status != "died" {
        return EventRoute::Ignore;
    }
    if event.name == primary {
        EventRoute::PrimaryDied
    } else {
        EventRoute::SidecarDied(event.name)
    }
}

/// Read this session's `died` events until the primary dies or the stream ends.
/// Filtered by `event=died` + the session label, and replayed from `since` so a
/// death during setup is caught. A sidecar-death line is emitted only for a
/// name in the shared list: `--since` replays setup-time deaths of warn-path
/// sidecars that were created then dropped and never registered, which must
/// stay silent.
async fn watch_events(
    primary: String,
    sid: String,
    since: SystemTime,
    died: CancellationToken,
    sidecars: Arc<Mutex<Vec<String>>>,
) {
    let since_secs = since
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    let label_filter = format!("label={LABEL_SESSION}={sid}");
    let mut child = match tokio::process::Command::new("podman")
        .args(["events", "--since", &since_secs])
        .args(["--filter", "event=died", "--filter", &label_filter])
        .args(["--format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(
                target: "outrig::cli::watcher",
                "podman events for session {sid} failed to spawn: {e}; auto-reap disabled"
            );
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        tracing::warn!(
            target: "outrig::cli::watcher",
            "podman events for session {sid} produced no stdout; auto-reap disabled"
        );
        return;
    };

    let mut lines = tokio::io::BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match route_event(&line, &primary) {
                EventRoute::PrimaryDied => {
                    // Snapshot when the death fires, not at arm time, so
                    // dynamically added sidecars are reaped too.
                    let names = sidecars.lock().expect("sidecar name list lock").clone();
                    for name in &names {
                        force_remove_detached(name);
                    }
                    eprintln!(
                        "[outrig] primary container {primary} exited unexpectedly; \
                         reaping {} sidecar container(s)",
                        names.len()
                    );
                    died.cancel();
                    // Stop reading: the reap generates further `died` events we
                    // do not want to log as spontaneous sidecar deaths.
                    return;
                }
                EventRoute::SidecarDied(name) => {
                    let known = sidecars
                        .lock()
                        .expect("sidecar name list lock")
                        .contains(&name);
                    if known {
                        eprintln!(
                            "[outrig] sidecar container {name} exited; \
                             its MCP tools will return errors until the session ends"
                        );
                        tracing::warn!(
                            target: "outrig::cli::watcher",
                            "sidecar container {name} exited mid-session"
                        );
                    }
                }
                EventRoute::Ignore => {}
            },
            // Stream ended without a primary death. Degrade to today's
            // single-container "no auto-reap"; never cancel -- the session is
            // still alive.
            Ok(None) => {
                tracing::warn!(
                    target: "outrig::cli::watcher",
                    "podman events stream for session {sid} ended; auto-reap disabled"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    target: "outrig::cli::watcher",
                    "reading podman events for session {sid} failed: {e}; auto-reap disabled"
                );
                return;
            }
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
/// (e.g. no such container) is treated as "already gone". Used by attach-mode
/// `outrig mcp`, whose single borrowed container needs no events stream.
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

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: &str = "outrig-20260713T173119-fa97";

    #[test]
    fn route_event_matches_primary_by_name() {
        let line = r#"{"ContainerExitCode":137,"ID":"7d3c","Image":"localhost/x:latest","Name":"outrig-20260713T173119-fa97","Status":"died","Time":"2026-07-13T11:31:49-06:00","Type":"container","Attributes":{"io.buildah.version":"1.33.7"}}"#;
        assert_eq!(route_event(line, PRIMARY), EventRoute::PrimaryDied);
    }

    #[test]
    fn route_event_routes_other_names_to_sidecar() {
        let line = r#"{"ContainerExitCode":137,"ID":"4ccc","Image":"localhost/x:latest","Name":"outrig-20260713T173119-fa97-tools","Status":"died","Time":"2026-07-13T11:31:47-06:00","Type":"container","Attributes":{"org.outrig.session":"20260713T173119-fa97","org.outrig.sidecar":"tools"}}"#;
        assert_eq!(
            route_event(line, PRIMARY),
            EventRoute::SidecarDied("outrig-20260713T173119-fa97-tools".to_string())
        );
    }

    #[test]
    fn route_event_ignores_malformed_json() {
        assert_eq!(route_event("not json", PRIMARY), EventRoute::Ignore);
        assert_eq!(route_event("", PRIMARY), EventRoute::Ignore);
    }

    #[test]
    fn route_event_ignores_non_died_and_non_container_events() {
        let started =
            r#"{"Name":"outrig-20260713T173119-fa97","Status":"start","Type":"container"}"#;
        assert_eq!(route_event(started, PRIMARY), EventRoute::Ignore);
        let image_event =
            r#"{"Name":"outrig-20260713T173119-fa97","Status":"died","Type":"image"}"#;
        assert_eq!(route_event(image_event, PRIMARY), EventRoute::Ignore);
    }
}
