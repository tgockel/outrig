//! The built-in default image-config, exercised without a container runtime.
//!
//! Deliberately not `e2e`-gated. CI compiles the e2e suite but never runs it
//! (`.github/workflows/ci.yml`), so the assertions that keep this feature
//! honest day to day have to be reachable without podman.
//!
//! Two mechanisms make that true. Every assertion here is about *name
//! resolution*, which happens before the first podman call -- and for the
//! commands that would go on to pull, [`run_outrig`] puts a `podman` and
//! `buildah` that fail immediately on `PATH`. Without that stub,
//! `outrig build --image outrig-default` really pulls ~300 MB on a cold
//! machine and then trips the timeout. It also pins `XDG_CACHE_HOME` to a
//! tempdir so the built-in's materialized Dockerfile never touches the
//! developer's real `~/.cache/outrig`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// A global config that resolves a model, so a run gets past model wiring and
/// into the image cascade. The provider is never contacted.
const GLOBAL_WITH_MODEL: &str = r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "http://127.0.0.1:1/v1"
api-key  = "${OUTRIG_TEST_KEY}"

[models.fast]
provider   = "openai"
identifier = "test-model"
"#;

/// A `PATH` whose `podman` and `buildah` exit non-zero immediately, so image
/// probes and pulls fail instantly instead of hitting the network.
fn stub_runtime_path(dir: &Path) -> std::ffi::OsString {
    for name in ["podman", "buildah"] {
        let stub = dir.join(name);
        std::fs::write(&stub, "#!/bin/sh\nexit 1\n").expect("write runtime stub");
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod runtime stub");
    }
    let mut path = std::ffi::OsString::from(dir);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    path
}

/// Run `outrig` in `cwd` and return whether it succeeded, plus its stderr.
async fn run_outrig(cwd: &Path, args: &[&str]) -> (bool, String) {
    let stubs = tempfile::tempdir().expect("tempdir stubs");
    let cache = tempfile::tempdir().expect("tempdir cache");

    let output = timeout(
        TEST_TIMEOUT,
        Command::new(env!("CARGO_BIN_EXE_outrig"))
            .args(args)
            .current_dir(cwd)
            .env("OUTRIG_TEST_KEY", "test-key")
            .env("PATH", stub_runtime_path(stubs.path()))
            .env("XDG_CACHE_HOME", cache.path())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("outrig timed out")
    .expect("spawn outrig");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Write a repo config and a global config into a fresh tempdir repo.
fn repo_with(config_toml: &str) -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("tempdir repo");
    let cfg_dir = repo.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).expect("create config dir");
    std::fs::write(cfg_dir.join("config.toml"), config_toml).expect("write repo config");
    std::fs::write(repo.path().join("global.toml"), GLOBAL_WITH_MODEL).expect("write global");
    repo
}

fn global_arg(repo: &Path) -> String {
    repo.join("global.toml")
        .to_str()
        .expect("utf-8")
        .to_string()
}

/// The headline: in a directory with no repo config and no global config, the
/// blocker is the *model*, not the image. Before the built-in default existed
/// this run had two blockers; the image one is gone.
#[tokio::test]
async fn a_bare_directory_now_fails_only_on_the_model() {
    let repo = tempfile::tempdir().expect("tempdir repo");
    let sessions = tempfile::tempdir().expect("tempdir sessions");
    let absent = repo.path().join("no-such-global.toml");

    let (ok, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            absent.to_str().expect("utf-8"),
            "--session-root",
            sessions.path().to_str().expect("utf-8"),
            "run",
        ],
    )
    .await;

    assert!(
        !ok,
        "a run with no model anywhere must still fail:\n{stderr}"
    );
    assert!(
        stderr.contains("no model selected"),
        "the remaining blocker should be the model: {stderr}"
    );
    assert!(
        !stderr.contains("no --image"),
        "the image rung must no longer be a blocker: {stderr}"
    );
}

/// `--all` means "every image-config *you* declared". Pulling and building
/// outrig's fallback in every repo would be a surprise, and an expensive one.
#[tokio::test]
async fn build_all_never_targets_the_built_in() {
    let repo = repo_with("[images.mine]\nimage-name = \"docker.io/library/alpine:latest\"\n");

    let (_, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            &global_arg(repo.path()),
            "build",
            "--all",
        ],
    )
    .await;

    assert!(
        !stderr.contains("outrig-default"),
        "--all must not reach outrig's built-in images: {stderr}"
    );
}

/// The built-in stays reachable by name, which is how the first-run pull and
/// build get paid deliberately rather than inside someone's first session.
/// Asserted through the *absence* of the "does not match any" error: resolving
/// the name is the behavior under test, not what podman then does with it.
#[tokio::test]
async fn the_built_in_is_reachable_by_name_from_build() {
    let repo = repo_with("");

    let (_, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            &global_arg(repo.path()),
            "build",
            "--image",
            "outrig-default",
        ],
    )
    .await;

    assert!(
        !stderr.contains("does not match any [images.<name>]"),
        "`outrig build --image outrig-default` must resolve the built-in: {stderr}"
    );
}

/// `--image outrig-default` must mean the same thing to `run` as it does to
/// `build`. It is the name the banner prints, so it is the first thing a user
/// will type -- and without injection on this path it would fall through to
/// the raw-local-ref rule and fail at podman instead.
#[tokio::test]
async fn the_built_in_is_reachable_by_name_from_run() {
    let repo = repo_with("");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let (_, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            &global_arg(repo.path()),
            "--session-root",
            sessions.path().to_str().expect("utf-8"),
            "run",
            "--image",
            "outrig-default",
        ],
    )
    .await;

    assert!(
        !stderr.contains("did not match any [images.<name>]"),
        "`outrig run --image outrig-default` must resolve the built-in, not fall \
         through to the raw-local-ref rule: {stderr}"
    );
}

/// A repo that declares the reserved name keeps it. The built-in steps aside
/// and says so, rather than silently taking the name over or half-merging into
/// it.
#[tokio::test]
async fn a_user_declared_reserved_name_wins_and_is_reported() {
    let repo =
        repo_with("[images.outrig-default]\nimage-name = \"docker.io/library/alpine:latest\"\n");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let (_, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            &global_arg(repo.path()),
            "--session-root",
            sessions.path().to_str().expect("utf-8"),
            "run",
        ],
    )
    .await;

    assert!(
        stderr.contains("shadows outrig's built-in default"),
        "a shadowed built-in must be reported, not silent: {stderr}"
    );
    assert!(
        !stderr.contains("using outrig's built-in default"),
        "the built-in must not also claim to be in play: {stderr}"
    );
}

/// Declaring a reserved *sidecar* name vetoes injection but leaves no
/// `[images.outrig-default]` behind. The session must say what it lacks rather
/// than name a block the user never wrote.
#[tokio::test]
async fn a_shadowing_sidecar_reports_the_missing_image_not_a_phantom_block() {
    let repo =
        repo_with("[sidecars.outrig-default-fs]\nimage = \"docker.io/library/alpine:latest\"\n");
    let sessions = tempfile::tempdir().expect("tempdir sessions");

    let (ok, stderr) = run_outrig(
        repo.path(),
        &[
            "--global-config",
            &global_arg(repo.path()),
            "--session-root",
            sessions.path().to_str().expect("utf-8"),
            "run",
        ],
    )
    .await;

    assert!(!ok, "nothing resolves an image here:\n{stderr}");
    assert!(
        stderr.contains("shadowed by a [sidecars.<name>] block"),
        "the error should explain the veto: {stderr}"
    );
    assert!(
        !stderr.contains("image-config \"outrig-default\" does not match"),
        "must not name a block the user never wrote: {stderr}"
    );
}
