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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cli::engine;
use crate::error::{CliError, Result};
use outrig::container::LABEL_SESSION;

/// Podman container label identifying one sidecar container of one session.
///
/// Stamped by [`super::session_setup`] at launch, so the primary-death reap can
/// select the container it means instead of asking podman for a *name*. A name
/// is a request, not a claim -- podman hands one back the moment a container
/// exits, and the reap runs exactly when containers are exiting -- while this
/// value carries 128 random bits that nothing else on the machine has.
///
/// Distinct from [`outrig::container`]'s `org.outrig.attempt` for a reason that
/// is about reach, not lifetime: the attempt token is private to the library
/// and `Container` exposes no accessor for it, so the CLI cannot select on it.
/// Minting one here costs a second value that means nearly the same thing;
/// exposing the library's would move `crates/outrig/public-api.txt`, which is a
/// release gate. `0002-55`'s `## Decisions` records that trade and the
/// conditions under which the split should be collapsed.
///
/// Declared here rather than beside `LABEL_SESSION` and `LABEL_SIDECAR` for the
/// same reason, and that is the cost of the split: the `org.outrig.*` namespace
/// otherwise has one owner, and `reject_reserved_labels` guards only the
/// library's own key, so nothing stops a library caller stamping this one.
pub const LABEL_INSTANCE: &str = "org.outrig.instance";

/// A sidecar container the watcher is responsible for: the name it was given,
/// and the [`LABEL_INSTANCE`] value its removal selects on.
///
/// The instance is not optional. Every sidecar this build starts is stamped
/// with one before it exists, and the watcher is always armed by the process
/// that started them -- so there is no state in which the watcher holds a
/// sidecar it cannot select. Making it an `Option` would add a by-name
/// fallback that nothing reaches, which is how the defect this replaced
/// survived as long as it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarRef {
    pub name: String,
    pub instance: String,
}

impl SidecarRef {
    pub fn new(name: String, instance: String) -> Self {
        Self { name, instance }
    }

    /// The label selector this sidecar's removal filters on. A method rather
    /// than something [`reap`] builds inline, so it can be asserted on without
    /// running an engine.
    fn selector(&self) -> String {
        format!("{LABEL_INSTANCE}={}", self.instance)
    }
}

/// Watches a session's containers. Spawned only for sessions that declare
/// sidecars (started or `start = "manual"`) -- a single-container session
/// keeps today's behavior. The sidecar list is shared with the events reader
/// so a `/sidecar add` mid-session is covered from the moment it is
/// [registered](SessionWatcher::register_sidecar); the label-filtered stream
/// already delivers the new container's death, so registration only has to
/// record what the primary-death reap needs in order to remove it.
#[derive(Debug)]
pub struct SessionWatcher {
    died: CancellationToken,
    sidecars: Arc<Mutex<Vec<SidecarRef>>>,
    task: JoinHandle<()>,
}

impl SessionWatcher {
    /// Arm the watcher: one `podman events` reader for the whole session. It
    /// reaps every registered sidecar and cancels [`SessionWatcher::primary_died`]
    /// when the primary dies, and logs any sidecar death. `since` (the session
    /// start) is replayed so a container that died between start and arm is not
    /// missed.
    pub fn spawn(
        primary: String,
        sidecars: Vec<SidecarRef>,
        sid: String,
        since: SystemTime,
    ) -> Self {
        let died = CancellationToken::new();
        let sidecars: Arc<Mutex<Vec<SidecarRef>>> = Arc::new(Mutex::new(sidecars));
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

    /// Cover one more sidecar container: record it, with the selector its
    /// removal will filter on, so the primary-death reap includes it. The
    /// session-filtered events stream already reports its death, so no new
    /// process is needed. Called mid-session by `/sidecar add`.
    pub fn register_sidecar(&mut self, sidecar: SidecarRef) {
        self.sidecars
            .lock()
            .expect("sidecar list lock")
            .push(sidecar);
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

/// Announce the primary's death, release everything waiting on it, and then
/// reap the sidecars.
///
/// **The order is the contract.** `died` is how the REPL and the MCP session
/// learn their primary is gone and begin tearing down, so cleanup must not sit
/// in front of it -- a `podman rm` that never answered was a session that never
/// ended. Cleanup is not this task's to finish in any case: `teardown` stops
/// every sidecar through its own handle and reports what it could not, so the
/// reap here is a fast path, and a fast path may be given up on or abandoned.
///
/// Separated from the events loop so that order can be asserted on without an
/// engine, an events stream, or a container.
async fn on_primary_died(
    primary: &str,
    doomed: Vec<SidecarRef>,
    died: &CancellationToken,
    engine_program: &'static str,
) {
    // Said before the reaping: the count is known as soon as the death is, and
    // announcing afterwards left the terminal silent for the whole window and
    // then reported finished work in the future tense.
    eprintln!(
        "[outrig] primary container {primary} exited unexpectedly; \
         reaping {} sidecar container(s)",
        doomed.len()
    );
    died.cancel();
    // Concurrently: each removal is a separate podman process against a
    // different container and nothing orders them, so awaiting them one by one
    // added a full podman startup per sidecar.
    futures_util::future::join_all(
        doomed
            .iter()
            .map(|sidecar| reap(sidecar, engine_program)),
    )
    .await;
}

/// How long one sidecar's reap gets before it is given up on.
///
/// This is a fast path racing a teardown that will stop the same container
/// through its own handle, so the budget only has to separate "slow" from
/// "wedged" -- generously, since a loaded machine can stretch a healthy
/// `podman rm` by an order of magnitude. What it must not do is wait forever:
/// the session has already been told its primary died and is on its way out.
const REAP_BUDGET: Duration = Duration::from_secs(10);

/// Remove one sidecar, by the selector it carries.
///
/// Awaited rather than detached so a removal that failed can be said out loud,
/// which the fire-and-forget form this replaced could never do -- but bounded,
/// because nothing downstream is waiting on the answer and everything
/// downstream was waiting on the token that is now cancelled before this runs.
async fn reap(sidecar: &SidecarRef, engine_program: &'static str) {
    if let Err(e) = engine::remove_by_label(engine_program, &sidecar.selector(), REAP_BUDGET).await
    {
        eprintln!("[outrig] could not remove container {}: {e}", sidecar.name);
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
    sidecars: Arc<Mutex<Vec<SidecarRef>>>,
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
                    let doomed = sidecars.lock().expect("sidecar list lock").clone();
                    on_primary_died(&primary, doomed, &died, "podman").await;
                    // Stop reading: the reap generates further `died` events we
                    // do not want to log as spontaneous sidecar deaths.
                    return;
                }
                EventRoute::SidecarDied(name) => {
                    let known = sidecars
                        .lock()
                        .expect("sidecar list lock")
                        .iter()
                        .any(|sidecar| sidecar.name == name);
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

    /// An "engine" that ignores its argv and never answers -- a wedged
    /// `podman rm`, which a real podman cannot be asked to be on demand.
    fn wedged_engine() -> &'static str {
        let dir = std::env::temp_dir().join(format!("outrig-wedged-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("wedged-engine");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").expect("write fake engine");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake engine");
        }
        Box::leak(path.to_string_lossy().into_owned().into_boxed_str())
    }

    /// The session is released the moment its primary is known dead, not when
    /// the cleanup that follows happens to finish.
    ///
    /// This token is what the REPL and the MCP session select on to learn the
    /// primary is gone; awaiting the reaps in front of it made one `podman rm`
    /// that never answered into a session that never ended. The reap here is
    /// wedged for a minute, so a regression cannot pass this by being fast.
    #[tokio::test]
    async fn the_primary_death_signal_does_not_wait_for_the_reaping() {
        let died = CancellationToken::new();
        let doomed = vec![SidecarRef::new(
            "outrig-test-tools".to_string(),
            "salt-tools".to_string(),
        )];

        let watching = died.clone();
        let reaping = tokio::spawn(async move {
            on_primary_died("outrig-test-primary", doomed, &watching, wedged_engine()).await;
        });

        // Well inside `REAP_BUDGET`, so this can only pass if the signal is
        // ahead of the cleanup rather than merely bounded by it.
        tokio::time::timeout(Duration::from_secs(2), died.cancelled())
            .await
            .expect("the primary-death token must fire before the reaping finishes");

        reaping.abort();
    }

    /// The reap selects the container it means, not a name podman has already
    /// handed back. A sidecar dies at exactly the moment the primary does, and
    /// `--rm` frees its name on the spot -- so a removal that resolves the
    /// name a moment later can reach whatever took it.
    #[test]
    fn a_reap_selects_the_instance_label_and_not_the_container_name() {
        let sidecar = SidecarRef::new(
            "outrig-20260713T173119-fa97-tools".to_string(),
            "d7f3a1-tools".to_string(),
        );

        let selector = sidecar.selector();
        assert_eq!(selector, "org.outrig.instance=d7f3a1-tools");
        assert!(
            !selector.contains(&sidecar.name),
            "the container name is what a replacement would share with it: {selector}"
        );
    }
}
