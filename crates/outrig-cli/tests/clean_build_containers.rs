//! `outrig clean --build-containers` against stub engines, for the listings
//! that race a removal.
//!
//! buildah and podman each list working containers in two steps -- every
//! container, then something about each one -- so a container removed between
//! the two fails the whole listing with `container not known`. Any build on the
//! machine finishing or being cancelled is enough, and the sweep has to take
//! that listing again rather than stop on it. Anything else an engine says is
//! still final, and so is the same failure once the attempts run out.
//!
//! The stubs print what the real engines printed, count how often they ran,
//! and are driven through the real binary, so what is pinned is what a user
//! of `clean` sees.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

mod common;
use common::{run_outrig_output, utf8};

/// Listings the sweep takes before it believes a `container not known`. Keep in
/// step with `LISTING_ATTEMPTS` in `cli/build_containers.rs`, and with the
/// "five" in the CHANGELOG entry.
const ATTEMPTS: usize = 5;

/// A working container, as `buildah containers --json` reports one.
const CONTAINER_NAME: &str = "alpine-working-container";
const BUILDAH_LISTING: &str = r#"[{"id":"213c716fa504aa1f0f4c1b1b6f0f0b7b2f9a1c0d0e0f0a0b0c0d0e0f0a0b0c0d","builder":true,"imagename":"docker.io/library/alpine:latest","containername":"alpine-working-container"}]"#;

/// Not `permission denied`, which a failure to run the stub at all also renders.
const UNREADABLE: &str = "Error: the store is unreadable";

/// Held for the whole of every test. Each one writes executables and then runs
/// them, and an executable still open for writing -- another thread's `fork`
/// copied the descriptor while the file was being written -- cannot be run
/// (`ETXTBSY`). Only these tests fork in this binary, so taking turns is
/// enough; a test added here must take it too, by building a [`Stubs`].
static STUB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Buildah,
    Podman,
}

impl Engine {
    const BOTH: [Engine; 2] = [Engine::Buildah, Engine::Podman];

    fn program(self) -> &'static str {
        match self {
            Engine::Buildah => "buildah",
            Engine::Podman => "podman",
        }
    }

    /// What the real engine printed when its listing lost the race: buildah's
    /// from the issue's CI log, podman's from `podman ps -a --external` run
    /// beside a loop creating and removing working containers.
    fn raced(self) -> &'static str {
        match self {
            Engine::Buildah => "Error: reading build containers: container not known",
            Engine::Podman => {
                "Error: determining buildah container for container \
                 f9063a68653cab591930fd2d51f164c9dcf34c3a0ddea7d15db4ac6433e3dbc0: \
                 container not known"
            }
        }
    }
}

/// A stub engine. Only the invocations matching the `is_listing` `case`
/// pattern count and can fail -- every buildah run, but of podman's only the
/// `--external` one, because `clean` also runs a plain `podman ps -a` that is
/// not this sweep's business. The first `failures` of them print `stderr` and
/// exit 125, as both engines do for an error; later ones print `listing`.
fn stub_script(is_listing: &str, failures: usize, stderr: &str, listing: &str) -> String {
    format!(
        r#"#!/bin/sh
case " $* " in {is_listing}) ;; *) printf '%s\n' '[]'; exit 0 ;; esac
runs=0
[ -f "$0.runs" ] && read -r runs < "$0.runs"
printf '%s\n' "$((runs + 1))" > "$0.runs"
if [ "$runs" -lt {failures} ]; then
  printf '%s\n' '{stderr}' >&2
  exit 125
fi
printf '%s\n' '{listing}'
"#
    )
}

fn write_executable(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write stub");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

/// Stub `buildah` and `podman`, the only entries on `PATH`, so a real engine
/// cannot be reached. One of them fails its first listings.
struct Stubs {
    dir: tempfile::TempDir,
    /// After `dir`, so the directory is removed while the lock is still held.
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl Stubs {
    /// `failing`'s first `failures` listings print `stderr` and fail; every
    /// other listing succeeds, and finds one container if it is buildah's.
    async fn new(failing: Engine, failures: usize, stderr: &str) -> Self {
        let lock = STUB_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir stubs");
        for engine in Engine::BOTH {
            let (is_listing, listing) = match engine {
                Engine::Buildah => ("*", BUILDAH_LISTING),
                Engine::Podman => (r#"*" --external "*"#, "[]"),
            };
            let failures = if engine == failing { failures } else { 0 };
            write_executable(
                &dir.path().join(engine.program()),
                &stub_script(is_listing, failures, stderr, listing),
            );
        }
        std::fs::create_dir(dir.path().join("sessions")).expect("session root");
        Stubs { dir, _lock: lock }
    }

    /// How many listings `engine` was asked for. Zero if it never ran.
    fn runs(&self, engine: Engine) -> usize {
        std::fs::read_to_string(self.dir.path().join(format!("{}.runs", engine.program())))
            .map(|count| count.trim().parse().expect("a run count"))
            .unwrap_or(0)
    }

    /// `outrig clean --build-containers` against the stubs. `--session-root`
    /// means the global config is never read, and nothing here is old enough
    /// to remove, so the sweep prompts for nothing.
    async fn clean(&self) -> std::process::Output {
        let root = self.dir.path().join("sessions");
        run_outrig_output(
            self.dir.path(),
            &["--session-root", utf8(&root), "clean", "--build-containers"],
            &[("PATH", self.dir.path())],
        )
        .await
    }
}

#[tokio::test]
async fn a_listing_that_raced_a_removal_is_taken_again() {
    for engine in Engine::BOTH {
        let stubs = Stubs::new(engine, 1, engine.raced()).await;
        let out = stubs.clean().await;
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            out.status.success(),
            "{engine:?}: the second listing succeeded, so the sweep should have:\n{stderr}"
        );
        assert!(
            stderr.contains(CONTAINER_NAME),
            "{engine:?}: the container the sweep listed is reported:\n{stderr}"
        );
        assert_eq!(
            stubs.runs(engine),
            2,
            "{engine:?}: one raced listing, then the one that succeeded"
        );
    }
}

#[tokio::test]
async fn a_listing_that_keeps_racing_stops_the_sweep_after_five_attempts() {
    for engine in Engine::BOTH {
        // Would succeed on a sixth attempt, so a sixth shows up as success.
        let stubs = Stubs::new(engine, ATTEMPTS, engine.raced()).await;
        let out = stubs.clean().await;
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            !out.status.success(),
            "{engine:?}: a listing that never gets through is an error:\n{stderr}"
        );
        assert!(
            stderr.contains("container not known"),
            "{engine:?}: the error is the engine's own:\n{stderr}"
        );
        assert_eq!(stubs.runs(engine), ATTEMPTS, "{engine:?}: attempts made");
    }
}

#[tokio::test]
async fn any_other_listing_failure_stops_the_sweep_at_once() {
    for engine in Engine::BOTH {
        let stubs = Stubs::new(engine, ATTEMPTS, UNREADABLE).await;
        let out = stubs.clean().await;
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            !out.status.success(),
            "{engine:?}: an unreadable store is an error:\n{stderr}"
        );
        assert!(
            stderr.contains("the store is unreadable"),
            "{engine:?}: the error is the engine's own:\n{stderr}"
        );
        assert_eq!(stubs.runs(engine), 1, "{engine:?}: attempts made");
    }
}
