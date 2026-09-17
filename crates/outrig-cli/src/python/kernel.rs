//! The host's half of the agent's Python session.
//!
//! [`PythonKernel::start`] execs the bundled interpreter inside the primary container and
//! speaks NDJSON to it over the exec's stdio: one JSON object per line, requests in on the
//! kernel's stdin, results and channel traffic out on its stdout. A driver task owns the child
//! and correlates replies, so the tool and the REPL can both hold a clone.
//!
//! ## What owns what, and what ends it
//!
//! The `Child` here is the host-side `podman exec` client, not the kernel. Killing it does not
//! stop the interpreter -- that runs under conmon inside the container and outlives its client
//! (see [`outrig::container::Container::exec_stdio`]). The kernel ends when the primary
//! container stops, during session teardown. Dropping this handle closes the kernel's stdin,
//! which is what makes it exit cleanly on the ordinary path.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use outrig::container::{Container, ExecOptions};

use crate::error::{OutrigError, Result};

use super::console::Console;
use super::payload::CONTAINER_PYTHON;

/// The in-container supervisor, passed to the interpreter on its command line. Shipping it as
/// argv rather than a second bind mount keeps it rebuilding with the binary; the cost is that a
/// traceback raised *inside the kernel* says `<string>`. Model-visible tracebacks name
/// `<execution>`, which the kernel sets explicitly.
const KERNEL_SOURCE: &str = include_str!("kernel.py");

/// How one submitted execution ended.
#[derive(Debug, Clone, Deserialize)]
pub struct ExecOutcome {
    /// Everything the execution wrote, already bounded by the kernel.
    #[serde(default)]
    pub output: String,
    /// The traceback, when the code raised. Not a transport failure: the tool ran, the code
    /// raised, and the traceback is what the model needs in order to fix it.
    #[serde(default)]
    pub error: Option<String>,
}

/// A bounded view of what the session holds. Names and type names only.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    pub globals: Vec<(String, String)>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub pending: BTreeMap<String, u64>,
}

/// How long an execution may go without proof of life before the kernel is
/// probed. Not an execution deadline: a build or a download can legitimately
/// hold the foreground for minutes, and a healthy kernel keeps its slot for as
/// long as it takes.
const LIVENESS_CHECK: Duration = Duration::from_secs(30);

/// How long the probe itself may take. A healthy kernel answers an inventory
/// immediately -- it is a dict walk -- so this only has to cover scheduling.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an interrupted execution has to unwind and report before the kernel
/// is declared unrecoverable.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

enum Request {
    Exec {
        source: String,
        reply: oneshot::Sender<Value>,
    },
    Inventory {
        reply: oneshot::Sender<Value>,
    },
    Message {
        text: String,
    },
    /// Handled on the kernel's reader thread rather than its event loop, which
    /// is the point: it is what un-wedges a loop that has stopped running
    /// callbacks. See `kernel.py`'s `_interrupt`.
    Interrupt,
}

/// A handle on the agent's Python session. Cheap to clone; every clone talks to the one
/// interpreter.
pub struct PythonKernel {
    requests: mpsc::UnboundedSender<Request>,
    /// Shared with the driver task, which writes through it when Python sends. The REPL writes
    /// its replies through the same one, which is what keeps the two from interleaving.
    console: Arc<Console>,
}

impl PythonKernel {
    /// Start the kernel inside `container` and wait for it to report ready.
    pub async fn start(container: &Container) -> Result<Arc<Self>> {
        let argv = vec![
            CONTAINER_PYTHON.to_string(),
            // Isolated mode: ignore PYTHONPATH, user site-packages, and any other Python
            // configuration the image happens to carry. The bundled library is the only one.
            "-I".to_string(),
            "-c".to_string(),
            KERNEL_SOURCE.to_string(),
        ];
        let mut child = container.exec_stdio(&argv, &ExecOptions::new()).await?;

        let stdin = child.stdin.take().ok_or_else(|| gone("stdin was not piped"))?;
        let stdout = child.stdout.take().ok_or_else(|| gone("stdout was not piped"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!("[python] {line}");
                }
            });
        }

        let mut lines = BufReader::new(stdout).lines();
        // The kernel announces itself once it has its loop, its capture pipe, and its reader
        // thread. Waiting for it here turns "the payload does not run in this image" into a
        // startup error rather than a hang on the first tool call.
        match lines.next_line().await {
            Ok(Some(line)) => {
                let ready: Value = serde_json::from_str(&line)
                    .map_err(|e| gone(&format!("unparsable greeting {line:?}: {e}")))?;
                if ready.get("t").and_then(Value::as_str) != Some("ready") {
                    return Err(gone(&format!("unexpected greeting {line:?}")));
                }
                tracing::debug!("python kernel ready: {ready}");
            }
            Ok(None) => return Err(gone("the interpreter exited before reporting ready")),
            Err(e) => return Err(gone(&format!("could not read the greeting: {e}"))),
        }

        let (tx, rx) = mpsc::unbounded_channel();
        // Built here rather than taken as an argument: `start` runs before the REPL exists, and
        // the driver task needs the console from its first line.
        let console = Arc::new(Console::new());
        tokio::spawn(drive(child, stdin, lines, rx, console.clone()));
        Ok(Arc::new(Self {
            requests: tx,
            console,
        }))
    }

    /// The session's writer. Anything else printing to the terminal during a turn goes through
    /// this, so a message Python sends cannot land inside it.
    pub fn console(&self) -> &Arc<Console> {
        &self.console
    }

    /// A handle with no interpreter behind it: every call fails with "the interpreter is
    /// gone". For unit tests that exercise the parts of a REPL session which never run Python.
    #[cfg(test)]
    pub fn detached() -> Arc<Self> {
        let (requests, _) = mpsc::unbounded_channel();
        Arc::new(Self {
            requests,
            console: Arc::new(Console::new()),
        })
    }

    /// Run `source` in the session and wait for the kernel's completion report.
    ///
    /// Synchronous Python that never yields -- `while True: pass`, a runaway
    /// comprehension -- blocks the kernel's event loop, and with it every path
    /// that could report the problem. Nothing below this would ever time out:
    /// the reply simply never comes, and the turn hangs rather than failing.
    ///
    /// So a wait without an answer is checked rather than trusted. The probe is
    /// what separates "wedged" from "slow": a healthy kernel answers an
    /// inventory while its foreground execution runs, because the loop is still
    /// turning. Only a kernel that has gone quiet is interrupted, which is what
    /// lets a legitimately long execution keep its slot indefinitely.
    pub async fn execute(&self, source: String) -> Result<ExecOutcome> {
        let (reply, mut answer) = oneshot::channel();
        self.requests
            .send(Request::Exec { source, reply })
            .map_err(|_| gone("the interpreter is gone"))?;

        let value = loop {
            if let Ok(answered) = tokio::time::timeout(LIVENESS_CHECK, &mut answer).await {
                break answered.map_err(|_| gone("the interpreter stopped answering"))?;
            }
            if tokio::time::timeout(PROBE_TIMEOUT, self.inventory())
                .await
                .is_ok_and(|probe| probe.is_ok())
            {
                // The loop is turning; the execution is just long.
                continue;
            }
            tracing::warn!("[python] the kernel stopped answering; interrupting the execution");
            self.requests
                .send(Request::Interrupt)
                .map_err(|_| gone("the interpreter is gone"))?;
            break tokio::time::timeout(INTERRUPT_GRACE, answer)
                .await
                .map_err(|_| {
                    gone(
                        "the execution did not stop when interrupted -- the interpreter is \
                         wedged in a call Python cannot break into, and the session has to end",
                    )
                })?
                .map_err(|_| gone("the interpreter stopped answering"))?;
        };
        serde_json::from_value(value).map_err(|e| gone(&format!("unreadable result: {e}")))
    }

    /// List what the session holds, without evaluating any of it.
    pub async fn inventory(&self) -> Result<Inventory> {
        let value = self.round_trip(|reply| Request::Inventory { reply }).await?;
        serde_json::from_value(value).map_err(|e| gone(&format!("unreadable inventory: {e}")))
    }

    /// Queue a line from the user on the `user` channel. It waits there until Python receives
    /// it -- delivery notifies, it does not consume, and the body never reaches the model
    /// except through code that asks for it.
    pub fn deliver_user(&self, text: String) -> Result<()> {
        self.requests
            .send(Request::Message { text })
            .map_err(|_| gone("the interpreter is gone"))
    }

    /// The observation block that opens a turn: what the session holds and what is waiting.
    /// With the conversation history discarded every turn, this is the only thing carrying
    /// state forward.
    pub async fn observation(&self) -> Result<String> {
        Ok(render_observation(&self.inventory().await?))
    }

    async fn round_trip(&self, make: impl FnOnce(oneshot::Sender<Value>) -> Request) -> Result<Value> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(make(reply))
            .map_err(|_| gone("the interpreter is gone"))?;
        answer.await.map_err(|_| gone("the interpreter stopped answering"))
    }
}

/// Owns the child and the two directions of the protocol.
async fn drive(
    mut child: tokio::process::Child,
    mut stdin: tokio::process::ChildStdin,
    mut lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    console: Arc<Console>,
) {
    let mut pending: HashMap<u64, oneshot::Sender<Value>> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        tokio::select! {
            request = requests.recv() => {
                let Some(request) = request else { break };
                let (wire, reply) = match request {
                    Request::Exec { source, reply } => {
                        let id = next_id;
                        next_id += 1;
                        (json!({"t": "exec", "id": id, "src": source}), Some((id, reply)))
                    }
                    Request::Inventory { reply } => {
                        let id = next_id;
                        next_id += 1;
                        (json!({"t": "inv", "id": id}), Some((id, reply)))
                    }
                    Request::Message { text } => (
                        json!({"t": "msg", "ch": "user",
                               "body": {"type": "UserText", "text": text}}),
                        None,
                    ),
                    Request::Interrupt => (json!({"t": "interrupt"}), None),
                };
                if let Some((id, reply)) = reply {
                    pending.insert(id, reply);
                }
                let mut line = wire.to_string();
                line.push('\n');
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    tracing::warn!("[python] unparsable kernel output: {line}");
                    continue;
                };
                match value.get("t").and_then(Value::as_str) {
                    Some("result") | Some("inv") => {
                        if let Some(id) = value.get("id").and_then(Value::as_u64)
                            && let Some(reply) = pending.remove(&id)
                        {
                            let _ = reply.send(value);
                        }
                    }
                    // Python talking to the user. This can arrive at any moment -- including
                    // from a background task, long after the turn that started it returned --
                    // so it goes through the console rather than straight to stdout.
                    Some("send") => {
                        let text = value
                            .get("body")
                            .and_then(|b| b.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if let Err(e) = console.agent_message(text).await {
                            tracing::warn!("[python] could not show an agent message: {e}");
                        }
                    }
                    _ => tracing::debug!("[python] {value}"),
                }
            }
        }
    }

    // Everything still waiting is waiting forever; dropping the senders turns each into an
    // error at its caller instead.
    pending.clear();
    let _ = child.start_kill();
}

/// Render the per-turn observation. Names and counts only -- a notification does not copy the
/// message body into the model's context.
pub fn render_observation(inventory: &Inventory) -> String {
    use std::fmt::Write as _;

    let mut out = String::from(
        "Python execution supports top-level await.\n\
         Global variables persist between executions.\n\
         Use help() to inspect runtime interfaces.\n",
    );

    let _ = write!(out, "\nChannels:\n");
    for name in &inventory.channels {
        let _ = writeln!(out, "    {name}");
    }

    let _ = write!(out, "\nGlobals:\n");
    if inventory.globals.is_empty() {
        let _ = writeln!(out, "    (none)");
    } else {
        let width = inventory
            .globals
            .iter()
            .map(|(name, _)| name.len())
            .max()
            .unwrap_or(0);
        for (name, kind) in &inventory.globals {
            let _ = writeln!(out, "    {name:<width$}  {kind}");
        }
    }

    if !inventory.pending.is_empty() {
        let _ = write!(out, "\nPending input:\n");
        for (name, count) in &inventory.pending {
            let plural = if *count == 1 { "message" } else { "messages" };
            let _ = writeln!(out, "    {name} ({count} {plural})");
        }
    }

    out
}

fn gone(detail: &str) -> crate::error::CliError {
    OutrigError::Configuration(format!("python kernel: {detail}")).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> Inventory {
        Inventory {
            globals: vec![
                ("downloads".to_string(), "Future".to_string()),
                ("criteria".to_string(), "dict".to_string()),
            ],
            channels: vec!["user".to_string()],
            pending: BTreeMap::from([("user".to_string(), 1)]),
        }
    }

    /// The kernel's wire shapes decode straight into these structs, so a change on either side
    /// that stops matching fails here rather than at a tool call.
    #[test]
    fn the_kernels_replies_decode_into_their_structs() {
        let inventory: Inventory = serde_json::from_value(json!({
            "t": "inv", "id": 4,
            "globals": [["downloads", "Future"], ["criteria", "dict"]],
            "channels": ["user"],
            "pending": {"user": 2},
        }))
        .expect("the inventory decodes");
        assert_eq!(inventory.globals[0], ("downloads".into(), "Future".into()));
        assert_eq!(inventory.channels, ["user"]);
        assert_eq!(inventory.pending["user"], 2);

        let raised: ExecOutcome = serde_json::from_value(json!({
            "t": "result", "id": 1, "status": "error",
            "output": "partial\n", "error": "ZeroDivisionError: division by zero",
        }))
        .expect("a raise decodes");
        assert_eq!(raised.output, "partial\n");
        assert_eq!(raised.error.as_deref(), Some("ZeroDivisionError: division by zero"));

        // A clean run carries a null error, which is an absence rather than a missing field.
        let clean: ExecOutcome = serde_json::from_value(json!({
            "t": "result", "id": 2, "status": "ok", "output": "42\n", "error": null,
        }))
        .expect("a clean run decodes");
        assert!(clean.error.is_none(), "got: {:?}", clean.error);
    }

    #[test]
    fn the_observation_names_what_the_session_holds() {
        let rendered = render_observation(&inventory());
        assert_eq!(
            rendered,
            "Python execution supports top-level await.\n\
             Global variables persist between executions.\n\
             Use help() to inspect runtime interfaces.\n\
             \n\
             Channels:\n    user\n\
             \n\
             Globals:\n    downloads  Future\n    criteria   dict\n\
             \n\
             Pending input:\n    user (1 message)\n"
        );
    }

    #[test]
    fn an_empty_session_says_so_rather_than_showing_a_blank() {
        let rendered = render_observation(&Inventory {
            channels: vec!["user".to_string()],
            ..Inventory::default()
        });
        assert!(rendered.contains("Globals:\n    (none)\n"), "got: {rendered}");
        // Nothing is waiting, so the section is absent rather than empty.
        assert!(!rendered.contains("Pending input"), "got: {rendered}");
    }

    #[test]
    fn several_waiting_messages_are_counted_not_quoted() {
        let rendered = render_observation(&Inventory {
            pending: BTreeMap::from([("user".to_string(), 3)]),
            ..inventory()
        });
        assert!(rendered.contains("user (3 messages)"), "got: {rendered}");
    }
}
