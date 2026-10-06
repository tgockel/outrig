//! What the crate's tests need to drive the real interpreter.
//!
//! `Interpreter::start` needs a container with the payload mounted. Running
//! the same program with the same arguments on the host needs neither podman
//! nor an image, so the host half and the agent loop are both tested against
//! the real interpreter this way.

use std::future::Future;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nix::sys::resource::{Resource, getrlimit, setrlimit};

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::process::Child;

use super::host::{ARGS, ExecId, Interpreter, InterpreterError, Outcome, PRIMARY, Report};
use super::payload;
use crate::events::Events;

/// How long any one step may take before a test fails rather than hangs.
const TIMEOUT: Duration = Duration::from_secs(20);

/// `step`, or a panic once it has taken longer than [`TIMEOUT`].
pub(crate) async fn within<F: Future>(step: F) -> F::Output {
    tokio::time::timeout(TIMEOUT, step)
        .await
        .unwrap_or_else(|_| panic!("a step took longer than {TIMEOUT:?}"))
}

/// A clean run that printed `output` and nothing else.
pub(crate) fn ok(output: &str) -> Outcome {
    Outcome::Ok(Report {
        output: output.to_string(),
        ..Report::default()
    })
}

/// The `HOME` an interpreter run on the host gets unless its test gives one of
/// its own: one directory per user under the system's temporary directory.
/// The interpreter makes pip's user site under `HOME` as it starts, and that
/// must never land in the real home of whoever runs the tests.
pub(crate) fn host_home() -> PathBuf {
    std::env::temp_dir().join(format!("outrig-test-home-{}", nix::unistd::getuid()))
}

/// The payload's interpreter, started as `Interpreter::start` starts it but on
/// the host, with `agent` as its argument when there is one.
pub(super) async fn spawn(agent: Option<&str>) -> Child {
    let python = payload::host_dir()
        .await
        .expect("the payload this build embedded")
        .join("bin/python3");
    tokio::process::Command::new(python)
        .args(ARGS)
        .args(agent)
        .env("HOME", host_home())
        .env_remove("PYTHONUSERBASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the interpreter starts")
}

/// A started interpreter addressing the primary agent.
pub(crate) async fn start_on_host() -> Interpreter {
    start_on_host_with(Events::off()).await
}

/// [`start_on_host`], recording to `events`.
pub(crate) async fn start_on_host_with(events: Events) -> Interpreter {
    within(Interpreter::from_child(spawn(Some(PRIMARY)).await, events))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
}

/// How a test starts the interpreter on the host. By default: in the test's
/// working directory, with [`host_home`] as `HOME`, under the ceiling it sets
/// itself.
#[derive(Default)]
pub(crate) struct Start<'a> {
    /// A soft `RLIMIT_DATA` already in place when it starts.
    pub(crate) ceiling: Option<u64>,
    /// Its working directory, which it takes for the workspace.
    pub(crate) dir: Option<&'a Path>,
    /// Its `HOME`, under which pip installs.
    pub(crate) home: Option<&'a Path>,
}

/// The payload's `python3` as the host-side harnesses run it: `HOME` set for
/// tests and pip's variable cleared, every stream piped, and a process group
/// of its own, so a harness's `Drop` reaches the children it starts.
pub(crate) fn python_command(home: Option<&Path>) -> Command {
    let mut command = Command::new(python());
    command
        .env("HOME", home.map_or_else(host_home, Path::to_path_buf))
        .env_remove("PYTHONUSERBASE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

/// The interpreter as `Interpreter::start` starts it, on the host, per `start`.
pub(crate) fn interpreter_command(start: &Start) -> Command {
    let mut command = python_command(start.home);
    command.args(ARGS).arg(PRIMARY);
    if let Some(dir) = start.dir {
        command.current_dir(dir);
    }
    if let Some(bytes) = start.ceiling {
        let (_, hard) = getrlimit(Resource::RLIMIT_DATA).expect("RLIMIT_DATA");
        // SAFETY: the closure runs between fork and exec, and makes one
        // async-signal-safe call that neither allocates nor takes a lock.
        unsafe {
            command.pre_exec(move || {
                setrlimit(Resource::RLIMIT_DATA, bytes, hard).map_err(std::io::Error::from)
            });
        }
    }
    command
}

/// Everything `pipe` yields, gathered on a thread of its own as it arrives.
pub(crate) fn capture(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let sink = Arc::new(Mutex::new(String::new()));
    let into = Arc::clone(&sink);
    std::thread::spawn(move || {
        let mut buf = [0; 4096];
        while let Ok(n @ 1..) = pipe.read(&mut buf) {
            into.lock()
                .expect("stderr lock")
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    sink
}

/// The embedded payload's interpreter, unpacked once for the whole binary.
pub(crate) fn python() -> &'static Path {
    static PYTHON: OnceLock<PathBuf> = OnceLock::new();
    PYTHON.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime to unpack the payload on");
        let dir = runtime
            .block_on(payload::host_dir())
            .expect("the payload this build embedded");
        dir.join("bin/python3")
    })
}

/// Python written indented inside a Rust test, dedented.
pub(crate) fn py(source: &str) -> String {
    let lines: Vec<&str> = source
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .collect();
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| line.get(indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------- a fake transport

/// What the fake transport says about itself once it closes.
pub(crate) const HUNG_UP: &str = "the fake hung up";

/// The interpreter's end of a transport, scripted by the test.
pub(crate) struct Fake {
    requests: Lines<BufReader<ReadHalf<DuplexStream>>>,
    pub(crate) replies: WriteHalf<DuplexStream>,
}

pub(crate) type HostEnd = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);

impl Fake {
    pub(crate) fn pair() -> (Self, HostEnd) {
        let (host, fake) = tokio::io::duplex(1 << 16);
        let (requests, replies) = tokio::io::split(fake);
        let fake = Self {
            requests: BufReader::new(requests).lines(),
            replies,
        };
        (fake, tokio::io::split(host))
    }

    /// A handle connected to a fake that has greeted.
    pub(crate) async fn connected() -> (Interpreter, Self) {
        Self::connected_with(Events::off()).await
    }

    /// [`Fake::connected`], recording to `events`. The host's request to
    /// observe, which recording sends first, is read here.
    pub(crate) async fn connected_with(events: Events) -> (Interpreter, Self) {
        let (mut fake, host) = Self::pair();
        fake.send(json!({"t": "ready", "agent": PRIMARY, "version": "3.13"}))
            .await;
        let observing = events.is_on();
        let interpreter = within(connect_with(host, events))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        if observing {
            fake.expect("observe").await;
        }
        (interpreter, fake)
    }

    pub(crate) async fn send(&mut self, message: Value) {
        self.replies
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("the host reads it");
    }

    pub(crate) async fn next(&mut self) -> Value {
        let line = within(self.requests.next_line())
            .await
            .expect("the host's requests")
            .expect("a request");
        serde_json::from_str(&line).expect("a request is JSON")
    }

    /// Read the next request, which must be of `kind`.
    pub(crate) async fn expect(&mut self, kind: &str) -> Value {
        let request = self.next().await;
        assert_eq!(request["t"], kind, "expected {kind}, got {request}");
        request
    }

    /// Read the next request, which must be execution `id`'s.
    pub(crate) async fn exec(&mut self, id: ExecId) {
        let request = self.expect("exec").await;
        assert_eq!(
            request["id"],
            json!(id),
            "expected execution {id}, got {request}"
        );
    }

    /// Answer the next request, which must be an inventory, with an empty one.
    pub(crate) async fn answer_inventory(&mut self) {
        let request = self.expect("inv").await;
        self.inventory(&request["id"]).await;
    }

    /// An empty inventory, as the answer to request `id`.
    pub(crate) async fn inventory(&mut self, id: &Value) {
        self.send(json!({
            "t": "inv", "agent": PRIMARY, "id": id, "globals": [], "total": 0, "more": 0
        }))
        .await;
    }

    pub(crate) async fn result(&mut self, id: ExecId, fields: Value) {
        let mut result = json!({
            "t": "result", "agent": PRIMARY, "id": id,
            "output": "", "dropped": 0, "error": null, "background": [],
        });
        for (key, value) in fields.as_object().expect("an object") {
            result[key] = value.clone();
        }
        self.send(result).await;
    }

    pub(crate) async fn ok(&mut self, id: ExecId, output: &str) {
        self.result(id, json!({"status": "ok", "output": output}))
            .await;
    }
}

pub(crate) async fn connect(host: HostEnd) -> Result<Interpreter, InterpreterError> {
    connect_with(host, Events::off()).await
}

/// [`connect`], recording to `events`.
pub(crate) async fn connect_with(
    (replies, requests): HostEnd,
    events: Events,
) -> Result<Interpreter, InterpreterError> {
    Interpreter::connect(replies, requests, async { HUNG_UP.to_string() }, events).await
}

/// An inventory, answered by the fake. The next request the fake sees must be
/// it, and once the host has its reply it has read every line before it.
pub(crate) async fn round_trip(interpreter: &Interpreter, fake: &mut Fake) {
    let (inventory, ()) = tokio::join!(within(interpreter.inventory()), fake.answer_inventory());
    inventory.expect("an inventory");
}

/// An image that ships no Python, so only the payload can answer.
#[cfg(feature = "e2e")]
pub(crate) const ALPINE: &str = "docker.io/library/alpine:latest";

/// Make sure [`ALPINE`] is present locally.
#[cfg(feature = "e2e")]
pub(crate) async fn pull_alpine() {
    crate::image::pull_image(&crate::image::ImageTag::new(ALPINE))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
}

// ---------------------------------------------------------------------------- pip

/// Python binding `wheel(directory, module)`, which writes a pure-Python wheel
/// whose one module holds `ANSWER = 42` and returns its path, and `pip(*args)`,
/// which runs the `pip` on `PATH` and raises with everything it said if it
/// fails. A wheel installed with `--no-index` needs no network.
pub(crate) const PIP_PROBE: &str = r#"
import os, subprocess, zipfile

def wheel(directory, module):
    dist = f"{module}-1.0.dist-info"
    path = os.path.join(directory, f"{module}-1.0-py3-none-any.whl")
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(f"{module}.py", "ANSWER = 42\n")
        z.writestr(f"{dist}/METADATA", f"Metadata-Version: 2.1\nName: {module}\nVersion: 1.0\n")
        z.writestr(
            f"{dist}/WHEEL",
            "Wheel-Version: 1.0\nGenerator: outrig-tests\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        )
        z.writestr(f"{dist}/RECORD", "")
    return path

def pip(*args):
    run = subprocess.run(
        ["pip", "--isolated", "--disable-pip-version-check", "--no-cache-dir", *args],
        capture_output=True,
        text=True,
    )
    if run.returncode:
        raise RuntimeError(f"pip {args} exited {run.returncode}:\n{run.stdout}{run.stderr}")
"#;
