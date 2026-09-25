//! What the crate's tests need to drive the real interpreter.
//!
//! `Interpreter::start` needs a container with the payload mounted. Running
//! the same program with the same arguments on the host needs neither podman
//! nor an image, so the host half and the agent loop are both tested against
//! the real interpreter this way.

use std::future::Future;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Child;

use super::host::{ARGS, Interpreter, Outcome, PRIMARY, Report};
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
