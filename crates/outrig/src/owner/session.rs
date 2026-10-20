//! One session a host program asked for: started from its request, driven by its commands, and
//! ended -- by the program, or by the engine once the program is gone.
//!
//! The start is one future -- the program's paths checked, the image ensured, the session
//! started -- raced against its interruption: the program gone, or a shutdown it sent before the
//! start answered, which is an entry it gave up on. Interrupted, the start is dropped where it
//! stands. Each of its steps arms the cleanup for what it creates before it creates it -- the
//! container's removal by its attempt, buildah's, the network attachment's undo, the bindings'
//! group -- and the drop hands that cleanup over to `supervise`, which this waits on until it has
//! run: nothing the start made is left when the session's task ends, or the engine says what may
//! be. A start that fails on its own runs its failure path, and what that path hands over is
//! waited for the same way, before the program is told why.
//!
//! Once started, the session answers the program's commands in order, each after it is applied,
//! and pushes its events, from `starting` on; a start that did not finish pushes only their end.
//! A shutdown the program asked for still answers `admission.close` while it runs, and holds the
//! program's grace (`program.rs`) while it does, as does the cleanup of an entry it gave up on.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::connection::Out;
use super::{LeftOpen, SessionEnd};
use crate::config::{Config, EventsMode, MountAccess, MountConfig, validate_session_paths};
use crate::events::Record;
use crate::harness::event::{Received, Subscription};
use crate::harness::{
    Closer, ContainerSpec, DEFAULT_DRAIN, Session, SessionBuilder, ShutdownReport, Stopped,
    container_spec,
};
use crate::image;
use crate::supervise::{Owed, Tracked};

/// How long the engine waits for what an interrupted or failed start handed over to be
/// cleaned up: past the reaper's own bound on a wedged cleanup and its retries.
const SETTLE: Duration = Duration::from_secs(90);
/// How many session ids are tried before the session root is taken to be unusable: an id
/// another session took first is minted again.
const ID_ATTEMPTS: usize = 16;
/// What the program is told of a start the engine stopped part way.
const INTERRUPTED: &str = "the session's start was interrupted";

/// What every session a program asks for is started from.
pub(crate) struct Context {
    pub(crate) config: Config,
    pub(crate) repo_root: PathBuf,
    /// How many shutdowns the program asked for are running; its grace waits while any is.
    pub(crate) shutdowns: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) testing: super::tests::Sessions,
}

/// A program's `session.start`.
pub(crate) struct Request {
    /// The program's number for the session.
    pub(crate) handle: u64,
    /// The request's id, which its answer carries.
    pub(crate) reply: Value,
    pub(crate) workspace: Option<PathBuf>,
    pub(crate) mounts: Vec<MountRequest>,
}

/// One mount a program asks for, as a `[[workspace.mounts]]` entry says it: the container path
/// its host path when not given, and read-only unless it says otherwise.
#[derive(Debug, Deserialize)]
pub(crate) struct MountRequest {
    host_path: PathBuf,
    container_path: Option<PathBuf>,
    #[serde(default)]
    access: MountAccess,
}

/// A program's request about a session it asked for.
pub(crate) enum Command {
    Close { reply: Value },
    Shutdown { reply: Value, grace: Duration },
}

/// Start the session `request` asks for and serve its commands until it has ended. What the
/// program did not see the end of is returned, for the engine to report.
pub(crate) async fn run(
    request: Request,
    mut commands: mpsc::UnboundedReceiver<Command>,
    context: Arc<Context>,
    out: Out,
) -> Option<LeftOpen> {
    let Request {
        handle,
        reply,
        workspace,
        mounts,
    } = request;
    let prepared =
        prepare(&context, workspace.as_deref(), &mounts).and_then(|(config, mounted)| {
            let (id, logs) = session_dir(&config)?;
            Ok((config, mounted, id, logs))
        });
    let (config, mounted, id, logs) = match prepared {
        Ok(prepared) => prepared,
        Err(why) => {
            out.send(json!({"t": "events.end", "session": handle}))
                .await;
            out.refuse(reply, why).await;
            refuse_the_rest(commands, &out).await;
            return None;
        }
    };

    let mut builder = SessionBuilder::new(config.clone(), None, None)
        .no_primary()
        .repo_root(&context.repo_root);
    if config.events.mode() == EventsMode::Record {
        builder = builder.record_events();
    }
    let events = builder.subscribe();
    #[cfg(test)]
    let builder = context.testing.adjust(builder, handle, &id, &logs);

    let mut start = Tracked::new(start(
        Arc::clone(&context),
        builder,
        config,
        mounted,
        id.clone(),
        logs,
    ));
    // How the start ended: with the session, or without it -- failed, saying why, or interrupted
    // by the program's shutdown, answered with its `reply`, or by the program's end.
    let mut closes = Vec::new();
    let began = loop {
        tokio::select! {
            started = &mut start => break started.map_err(|why| (why, None)),
            command = commands.recv() => match command {
                Some(Command::Close { reply }) => closes.push(reply),
                Some(Command::Shutdown { reply, .. }) => {
                    break Err((INTERRUPTED.to_string(), Some(reply)));
                }
                None => break Err((INTERRUPTED.to_string(), None)),
            },
            () = out.gone().cancelled() => break Err((INTERRUPTED.to_string(), None)),
        }
    };

    let session = match began {
        Ok(session) => {
            drop(start.owed());
            session
        }
        Err((why, asked)) => {
            // What the start made is stopped before the program is told. A shutdown the program
            // asks for meanwhile -- an entry it gave up on -- holds its grace while this waits,
            // as one of a session that runs does.
            let mut asked: Vec<Value> = asked.into_iter().collect();
            let mut held = (!asked.is_empty()).then(|| Asked::new(&context.shutdowns));
            #[cfg(test)]
            context.testing.settling(handle);
            let settling = settled(start.owed());
            tokio::pin!(settling);
            let stopped = loop {
                tokio::select! {
                    stopped = &mut settling => break stopped,
                    Some(command) = commands.recv() => match command {
                        Command::Close { reply } => closes.push(reply),
                        Command::Shutdown { reply, .. } => {
                            held.get_or_insert_with(|| Asked::new(&context.shutdowns));
                            asked.push(reply);
                        }
                    },
                }
            };
            drop(held);
            let why = match &stopped {
                Stopped::NotProven { reason } => {
                    format!("{why}; and what its start had made is not proven stopped: {reason}")
                }
                Stopped::Proven => why,
            };
            out.send(json!({"t": "events.end", "session": handle}))
                .await;
            let mut told = out.refuse(reply, why).await;
            for asked in asked {
                let line = json!({"t": "session.shutdown", "id": asked, "stopped": stopped});
                told &= out.send(line).await;
            }
            refuse_all(closes, &out, "the session did not start").await;
            refuse_the_rest(commands, &out).await;
            // An end the program was told of is its own, unless what the start made is not
            // proven stopped.
            return (!told || matches!(stopped, Stopped::NotProven { .. })).then_some(LeftOpen {
                id,
                end: SessionEnd::NotStarted(stopped),
            });
        }
    };

    out.send(json!({"t": "session.start", "id": reply, "session": {"id": id}}))
        .await;
    // Its events, from `starting` on, once the program knows the session.
    let source = format!("/outrig/session/{id}");
    let pump = tokio::spawn(pump(handle, events, out.clone(), source));
    #[cfg(test)]
    context.testing.started(handle, &session);
    let closer = session.closer();
    for reply in closes {
        close(&closer, reply, &out).await;
    }
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Close { reply }) => close(&closer, reply, &out).await,
                Some(Command::Shutdown { reply, grace }) => {
                    let shutdowns = &context.shutdowns;
                    let ended =
                        shut_down(session, grace, reply, commands, &out, pump, shutdowns).await;
                    return ended.map(|report| LeftOpen { id, end: SessionEnd::ShutDown(report) });
                }
                None => break,
            },
            () = out.gone().cancelled() => break,
        }
    }
    // The program is gone, and left the session open.
    session.close_admission();
    let report = session.shutdown(DEFAULT_DRAIN).await;
    let _ = pump.await;
    Some(LeftOpen {
        id,
        end: SessionEnd::ShutDown(report),
    })
}

/// A shutdown the program asked for, counted while it runs.
struct Asked<'a>(&'a AtomicUsize);

impl<'a> Asked<'a> {
    fn new(shutdowns: &'a AtomicUsize) -> Self {
        shutdowns.fetch_add(1, Ordering::Relaxed);
        Self(shutdowns)
    }
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Shut `session` down for the program, with `grace` as the drain deadline, answering an
/// `admission.close` that arrives meanwhile, and then every shutdown asked for with the report;
/// the program's grace is held while the shutdown runs, counted in `shutdowns`. The report, when
/// the program was gone before it could be answered.
async fn shut_down(
    session: Session,
    grace: Duration,
    reply: Value,
    mut commands: mpsc::UnboundedReceiver<Command>,
    out: &Out,
    pump: JoinHandle<()>,
    shutdowns: &AtomicUsize,
) -> Option<ShutdownReport> {
    let mut replies = vec![reply];
    let closer = session.closer();
    let report = {
        let _asked = Asked::new(shutdowns);
        let stopping = session.shutdown(grace);
        tokio::pin!(stopping);
        loop {
            tokio::select! {
                report = &mut stopping => break report,
                Some(command) = commands.recv() => match command {
                    Command::Close { reply } => close(&closer, reply, out).await,
                    Command::Shutdown { reply, .. } => replies.push(reply),
                },
            }
        }
    };
    // Every event, and the end of them, before the answer.
    let _ = pump.await;
    let answer = report_json(&report);
    let mut delivered = !out.gone().is_cancelled();
    for reply in replies {
        let line = json!({"t": "session.shutdown", "id": reply, "report": answer});
        delivered &= out.send(line).await;
    }
    commands.close();
    while let Some(command) = commands.recv().await {
        match command {
            // Admission is closed: the session is shut down.
            Command::Close { reply } => {
                out.send(json!({"t": "admission.close", "id": reply})).await;
            }
            Command::Shutdown { reply, .. } => {
                let line = json!({"t": "session.shutdown", "id": reply, "report": answer});
                out.send(line).await;
            }
        }
    }
    (!delivered).then_some(report)
}

/// Close the session's admission, and say so to `reply` once it is closed.
async fn close(closer: &Closer, reply: Value, out: &Out) {
    closer.close_admission();
    out.send(json!({"t": "admission.close", "id": reply})).await;
}

/// Answer each request in `replies` with `why`.
async fn refuse_all(replies: Vec<Value>, out: &Out, why: &str) {
    for reply in replies {
        out.refuse(reply, why).await;
    }
}

/// Answer every command still queued, and every one sent later, with why none will run.
async fn refuse_the_rest(mut commands: mpsc::UnboundedReceiver<Command>, out: &Out) {
    commands.close();
    while let Some(command) = commands.recv().await {
        let (Command::Close { reply } | Command::Shutdown { reply, .. }) = command;
        out.refuse(reply, "the session is not running").await;
    }
}

/// Wait for what a start handed over to be cleaned up, and say whether it is proven gone.
async fn settled(owed: Owed) -> Stopped {
    match owed.settled(SETTLE).await {
        Ok(()) => Stopped::Proven,
        Err(why) => Stopped::NotProven {
            reason: why.join("; "),
        },
    }
}

/// The configuration a session starts from: the engine's, with the program's workspace in place
/// of the configured one and its mounts added -- and whether a workspace is mounted. Checked, so
/// nothing starts on paths that cannot be mounted.
fn prepare(
    context: &Context,
    workspace: Option<&Path>,
    mounts: &[MountRequest],
) -> Result<(Config, bool), String> {
    let absolute = |path: &Path, what: &str| match path.is_absolute() {
        true => Ok(()),
        false => Err(format!("{what} {} is not an absolute path", path.display())),
    };
    let mut config = context.config.clone();
    if let Some(workspace) = workspace {
        absolute(workspace, "the workspace")?;
        config.workspace.set_host_path(workspace);
    }
    let mounted = config.workspace.declared_host_path().is_some();
    for mount in mounts {
        absolute(&mount.host_path, "the mount")?;
        let container = mount.container_path.as_deref().unwrap_or(&mount.host_path);
        absolute(container, "the mount's container path")?;
        config
            .workspace
            .mounts
            .push(MountConfig::new(&mount.host_path, container, mount.access));
    }
    validate_session_paths(&config, &context.repo_root, mounted).map_err(|e| e.to_string())?;
    Ok((config, mounted))
}

/// A new session's id and its `logs` directory, `<session-root>/<id>/logs`, the session's
/// directory created by this call alone: an id another session took first is minted again.
fn session_dir(config: &Config) -> Result<(String, PathBuf), String> {
    let root = config
        .session_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("outrig-sessions"));
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("cannot create the session root {}: {e}", root.display()))?;
    for _ in 0..ID_ATTEMPTS {
        let id = crate::container::mint_session_id();
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                let logs = dir.join("logs");
                std::fs::create_dir(&logs)
                    .map_err(|e| format!("cannot create {}: {e}", logs.display()))?;
                return Ok((id, logs));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", dir.display())),
        }
    }
    Err(format!(
        "no session id was free under {} in {ID_ATTEMPTS} tries",
        root.display()
    ))
}

/// The session's start: its image ensured as `outrig run-new` ensures one, then the session
/// started in a container of it.
async fn start(
    context: Arc<Context>,
    builder: SessionBuilder,
    config: Config,
    mounted: bool,
    id: String,
    logs: PathBuf,
) -> Result<Session, String> {
    #[cfg(test)]
    if context.testing.on_host() {
        return builder.start_on_host().await.map_err(|e| e.to_string());
    }
    builder.check().map_err(|e| e.to_string())?;
    let image = config.default_image.as_deref().ok_or_else(|| {
        "no image to run the session in: name one with `default-image`".to_string()
    })?;
    let block = config
        .images
        .get(image)
        .ok_or_else(|| format!("default-image {image:?} names no [images.{image}] block"))?;
    let repo_root = &context.repo_root;
    let tag = image::compute_tag_for(image, block, repo_root)
        .await
        .map_err(|e| e.to_string())?;
    let ensured = image::ensure_tagged_image_for(image, block, repo_root, &tag, false, None)
        .await
        .map_err(|e| e.to_string())?;
    let ContainerSpec { spec, .. } =
        container_spec(&config, image, Some(&ensured.tag), repo_root, logs)
            .await
            .map_err(|e| e.to_string())?;
    let spec = match mounted {
        true => spec,
        false => spec.without_workspace(),
    };
    builder
        .container(spec.with_session_id(id))
        .start()
        .await
        .map_err(|e| e.to_string())
}

/// Push the session's events to the program as `events.jsonl` records them, then say they have
/// ended. Stops once the program is gone.
async fn pump(handle: u64, mut events: Subscription, out: Out, source: String) {
    while let Some(received) = events.recv().await {
        let line = match received {
            Received::Event(event) => match serde_json::to_value(Record::of(&event, &source)) {
                Ok(record) => json!({"t": "event", "session": handle, "record": record}),
                Err(e) => {
                    tracing::warn!("encoding {} for the host program failed: {e}", event.kind());
                    json!({"t": "missed", "session": handle, "count": 1})
                }
            },
            Received::Missed(count) => json!({"t": "missed", "session": handle, "count": count}),
        };
        if !out.send(line).await {
            return;
        }
    }
    out.send(json!({"t": "events.end", "session": handle}))
        .await;
}

/// The report as the program reads it: its fields, and its verdict.
fn report_json(report: &ShutdownReport) -> Value {
    let mut value =
        serde_json::to_value(report).unwrap_or_else(|e| json!({"error": e.to_string()}));
    value["verdict"] = json!(report.verdict());
    value
}
