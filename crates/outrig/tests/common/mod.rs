//! Shared test helpers across library integration tests. Cargo treats
//! files in `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

#![allow(dead_code)] // each test binary uses a different subset

use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use outrig::Transcript;
use outrig::config::ImageConfig;
use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::ImageTag;

/// Build-source `ImageConfig` for a fixture directory that holds a
/// `Dockerfile` -- the shape nearly every `ensure_image` test wants.
///
/// Shared so this crate's gated test binaries name the shape once. Since
/// `ImageConfig` is `#[non_exhaustive]`, `tests/` cannot spell the literal at
/// all; the constructor is the whole construction path from out here.
pub fn fixture_build_config() -> ImageConfig {
    ImageConfig::from_dockerfile("Dockerfile", ".")
}

/// Install a best-effort tracing subscriber for integration tests that
/// surface process output under `--nocapture`.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Stock image for the user-bootstrap tests. Ships no `useradd`, `groupadd`,
/// or `getent`, which is exactly what makes it worth testing against.
pub const ALPINE: &str = "docker.io/library/alpine:latest";

pub fn pull_alpine() {
    run_capture(Command::new("podman").arg("pull").arg(ALPINE));
}

/// Start an `alpine:latest` container over `host_ws`, optionally recording the
/// podman transcript.
pub async fn start_alpine(host_ws: &Path, transcript: Option<Transcript>) -> Container {
    let tag = ImageTag(ALPINE.to_string());
    let launch = ContainerLaunchSpec::workspace(host_ws, Path::new("/workspace"));
    match transcript {
        None => Container::start(&tag, launch).await.expect("start"),
        Some(transcript) => {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "outrig-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            Container::start_named(&tag, launch, name, Some(transcript))
                .await
                .expect("start")
        }
    }
}

/// `podman exec --user=0:0 <name> ...` -- the container's own root, which is
/// what a test needs to read or edit `/etc` regardless of the bootstrap.
pub fn root_cmd(name: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.args(["exec", "--user=0:0"]).arg(name);
    cmd
}

/// Stdout of a command run as the container's root, which must succeed.
pub fn root_stdout(name: &str, argv: &[&str]) -> String {
    let out = run_capture(root_cmd(name).args(argv));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Add the `shadow` package, which supplies `useradd`/`groupadd`. Needed only
/// by tests that plant an entry themselves or exercise the `podman exec`
/// bootstrap -- never by the bootstrap under test.
pub fn install_shadow(name: &str) {
    run_capture(root_cmd(name).args(["apk", "add", "--no-cache", "shadow"]));
}

/// Name of the entry with numeric id `id` in a `passwd`- or `group`-shaped
/// file. Parsed here rather than shelled out to `getent`, which these images
/// need not have.
pub fn entry_for_id(text: &str, id: u32) -> Option<String> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        (fields.nth(1)? == id.to_string()).then(|| name.to_string())
    })
}

/// Read a child's stdout to end and require a clean exit.
pub async fn read_stdout(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncReadExt as _;

    let mut out = String::new();
    child
        .stdout
        .as_mut()
        .expect("stdout was piped")
        .read_to_string(&mut out)
        .await
        .expect("read stdout");
    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "child exited non-zero: {status:?}");
    out
}

pub fn run_capture(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("spawn command");
    assert!(
        output.status.success(),
        "command exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
