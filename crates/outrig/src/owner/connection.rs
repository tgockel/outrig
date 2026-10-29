//! The engine's end of the owner protocol (see the module above): the greeting, what writes to
//! the program, and the loop that reads its requests and routes each to the session it names.
//!
//! Nothing here waits on the program once it is gone. Every write also watches `gone`: a program
//! that exited may leave its read end with a process that never reads it, and a full pipe nobody
//! reads must not hold up the stopping of what the program left open.

use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::pipe;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::LeftOpen;
use super::session::{self, Command, Context, MountRequest, Request};
use crate::harness::DEFAULT_DRAIN;
use crate::python::host::next_line;
use crate::python::supervisor::quoted;

/// The version of the owner protocol this engine speaks.
pub(crate) const PROTOCOL: u64 = 2;
/// How long a program has to greet once it is started: its interpreter's start and `outrig`'s
/// import, which take well under a second.
const GREETING: Duration = Duration::from_secs(30);
/// The longest line the engine reads from the program; a longer one is discarded with a
/// warning. A request's body, or an agent class instance's inputs, are in one.
const LINE_MAX: usize = 1 << 20;
/// The longest drain a program may ask a session's shutdown for: an hour, so a mistyped grace
/// cannot hold a session open for days. The engine's own steps after the drain are not counted.
const DRAIN_MAX: Duration = Duration::from_secs(3600);

/// Read the program's greeting and answer it. A program that does not greet, or speaks another
/// version, fails, with why.
pub(crate) async fn greet(
    reads: &mut BufReader<pipe::Receiver>,
    writes: &mut pipe::Sender,
) -> Result<(), String> {
    let mut line = Vec::new();
    match tokio::time::timeout(GREETING, next_line(reads, &mut line, LINE_MAX)).await {
        Err(_) => return Err(format!("it did not connect within {GREETING:?}")),
        Ok(Ok(Some(0))) => {}
        Ok(Ok(Some(_))) => return Err("its greeting was too long".to_string()),
        Ok(Ok(None) | Err(_)) => return Err("it ended before it connected".to_string()),
    }
    let not_a_greeting = || format!("its first line was not a greeting: {:?}", quoted(&line));
    let Ok(Value::Object(hello)) = serde_json::from_slice::<Value>(&line) else {
        return Err(not_a_greeting());
    };
    if hello.get("t").and_then(Value::as_str) != Some("hello") {
        return Err(not_a_greeting());
    }
    let id = hello.get("id").cloned().unwrap_or(Value::Null);
    let answer = match hello.get("protocol").and_then(Value::as_u64) {
        Some(PROTOCOL) => Ok(json!({"t": "hello", "id": id, "protocol": PROTOCOL})),
        other => Err(format!(
            "it speaks version {} of the owner protocol, and this engine speaks version \
             {PROTOCOL}: its `outrig` package is not this build's",
            other.map_or_else(|| "(none)".to_string(), |v| v.to_string())
        )),
    };
    let line = match &answer {
        Ok(line) => line.clone(),
        Err(why) => json!({"t": "error", "id": id, "error": why}),
    };
    let mut bytes = line.to_string().into_bytes();
    bytes.push(b'\n');
    let written = tokio::time::timeout(GREETING, writes.write_all(&bytes)).await;
    answer?;
    match written {
        Ok(Ok(())) => Ok(()),
        _ => Err("it did not take the engine's greeting".to_string()),
    }
}

/// What writes to the program, a line at a time, and `gone`. A program that does not read makes
/// the event pumps wait on it -- their subscriptions lose what they must, counted -- rather than
/// the engine's memory grow.
#[derive(Clone)]
pub(crate) struct Out {
    writes: Arc<Mutex<pipe::Sender>>,
    gone: CancellationToken,
}

impl Out {
    /// The writer for `writes`, which stops once `gone` is cancelled.
    pub(crate) fn new(writes: pipe::Sender, gone: CancellationToken) -> Self {
        Self {
            writes: Arc::new(Mutex::new(writes)),
            gone,
        }
    }

    /// Write `line` to the program, unless it is gone first. Whether it was written.
    pub(crate) async fn send(&self, line: Value) -> bool {
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        tokio::select! {
            biased;
            () = self.gone.cancelled() => false,
            written = async { self.writes.lock().await.write_all(&bytes).await } => match written {
                Ok(()) => true,
                Err(e) => {
                    tracing::debug!("writing to the host program failed: {e}");
                    false
                }
            },
        }
    }

    /// Answer request `id` with an error saying `why`.
    pub(crate) async fn refuse(&self, id: Value, why: impl Display) -> bool {
        self.send(json!({"t": "error", "id": id, "error": why.to_string()}))
            .await
    }

    /// Cancelled once the program is gone.
    pub(crate) fn gone(&self) -> &CancellationToken {
        &self.gone
    }
}

/// `session.start`'s fields.
#[derive(Deserialize)]
struct Start {
    session: u64,
    workspace: Option<PathBuf>,
    #[serde(default)]
    mounts: Vec<MountRequest>,
}

/// Read the program's requests until it is gone, then wait for every session it asked for to
/// end, and publish what it left open.
pub(crate) async fn serve(
    mut reads: BufReader<pipe::Receiver>,
    out: Out,
    context: Arc<Context>,
    view: Value,
    ended: watch::Sender<Option<Vec<LeftOpen>>>,
) {
    let mut sessions: HashMap<u64, mpsc::UnboundedSender<Command>> = HashMap::new();
    let mut started = JoinSet::new();
    let mut line = Vec::new();
    loop {
        let read = tokio::select! {
            () = out.gone().cancelled() => break,
            read = next_line(&mut reads, &mut line, LINE_MAX) => read,
        };
        match read {
            Ok(Some(0)) => {}
            Ok(Some(cut)) => {
                tracing::warn!(
                    "ignored a line of more than {LINE_MAX} bytes from the host program \
                     ({cut} over)"
                );
                continue;
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("reading from the host program failed: {e}");
                break;
            }
        }
        let Ok(Value::Object(request)) = serde_json::from_slice::<Value>(&line) else {
            tracing::warn!(
                "ignored a line from the host program that is not a request: {:?}",
                quoted(&line)
            );
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        match request.get("t").and_then(Value::as_str) {
            Some("config") => {
                out.send(json!({"t": "config", "id": id, "config": view}))
                    .await;
            }
            Some("session.start") => {
                match serde_json::from_value::<Start>(Value::Object(request)) {
                    Err(e) => {
                        out.refuse(id, format!("a session.start the engine cannot read: {e}"))
                            .await;
                    }
                    Ok(start) if sessions.contains_key(&start.session) => {
                        let why = format!("session {} is already the program's", start.session);
                        out.refuse(id, why).await;
                    }
                    Ok(start) => {
                        let (commands, received) = mpsc::unbounded_channel();
                        sessions.insert(start.session, commands);
                        let request = Request {
                            handle: start.session,
                            reply: id,
                            workspace: start.workspace,
                            mounts: start.mounts,
                        };
                        started.spawn(session::run(
                            request,
                            received,
                            Arc::clone(&context),
                            out.clone(),
                        ));
                    }
                }
            }
            Some(kind @ ("admission.close" | "session.shutdown")) => {
                let command = match kind {
                    "admission.close" => Ok(Command::Close { reply: id.clone() }),
                    _ => grace(&request).map(|grace| Command::Shutdown {
                        reply: id.clone(),
                        grace,
                    }),
                };
                let routed = command.and_then(|command| route(&sessions, &request, command));
                if let Err(why) = routed {
                    out.refuse(id, why).await;
                }
            }
            Some(kind)
                if kind.starts_with("agent.") || matches!(kind, "ask" | "withdraw" | "verdict") =>
            {
                let command = Command::Agent(Value::Object(request.clone()));
                // A notice, which carries no id, is not answered.
                if let Err(why) = route(&sessions, &request, command)
                    && !id.is_null()
                {
                    out.refuse(id, why).await;
                }
            }
            kind => {
                out.refuse(id, format!("the engine has no request {kind:?}"))
                    .await;
            }
        }
    }
    out.gone().cancel();
    drop(sessions);
    let mut left_open = Vec::new();
    while let Some(ended) = started.join_next().await {
        match ended {
            Ok(Some(left)) => left_open.push(left),
            Ok(None) => {}
            Err(e) => tracing::warn!("a host program's session task failed: {e}"),
        }
    }
    ended.send_replace(Some(left_open));
}

/// Hand `command` to the session `request` names.
fn route(
    sessions: &HashMap<u64, mpsc::UnboundedSender<Command>>,
    request: &Map<String, Value>,
    command: Command,
) -> Result<(), String> {
    let Some(handle) = request.get("session").and_then(Value::as_u64) else {
        return Err("the request names no session".to_string());
    };
    let Some(session) = sessions.get(&handle) else {
        return Err(format!("the program has no session {handle}"));
    };
    session
        .send(command)
        .map_err(|_| format!("session {handle} has ended"))
}

/// A `session.shutdown`'s drain deadline: [`DEFAULT_DRAIN`] when `null`, otherwise a finite
/// number of seconds from 0 to [`DRAIN_MAX`].
fn grace(request: &Map<String, Value>) -> Result<Duration, String> {
    let Some(grace) = request.get("grace").filter(|grace| !grace.is_null()) else {
        return Ok(DEFAULT_DRAIN);
    };
    grace
        .as_f64()
        .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
        .filter(|grace| *grace <= DRAIN_MAX)
        .ok_or_else(|| {
            format!(
                "a session's grace is a number of seconds from 0 to {}, not {grace}",
                DRAIN_MAX.as_secs()
            )
        })
}
