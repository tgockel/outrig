//! What the crate's tests need to drive the real interpreter.
//!
//! `Interpreter::start` needs a container with the payload mounted. Running
//! the same program with the same arguments on the host needs neither podman
//! nor an image, so the host half and the agent loop are both tested against
//! the real interpreter this way.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::process::Child;

use super::host::{ARGS, ExecId, Interpreter, InterpreterError, Outcome, PRIMARY, Report};
use super::payload;

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
    within(Interpreter::from_child(spawn(Some(PRIMARY)).await))
        .await
        .unwrap_or_else(|e| panic!("{e}"))
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
        let (mut fake, host) = Self::pair();
        fake.send(json!({"t": "ready", "agent": PRIMARY, "version": "3.13"}))
            .await;
        let interpreter = within(connect(host))
            .await
            .unwrap_or_else(|e| panic!("{e}"));
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

pub(crate) async fn connect((replies, requests): HostEnd) -> Result<Interpreter, InterpreterError> {
    Interpreter::connect(replies, requests, async { HUNG_UP.to_string() }).await
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
