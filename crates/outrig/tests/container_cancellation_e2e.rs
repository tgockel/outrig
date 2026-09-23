//! What a cancelled container creation leaves in a real engine.
//!
//! `tests/cancellation.rs` covers the same windows against shell-script fakes
//! and says in its header what that cannot reach: the fakes prove outrig kills
//! the client it spawned and issues the removal it owes, not that the *engine*
//! then holds no container. 0002-39 deferred the engine half here by name --
//! "After a canceled create, `podman ps -a` lists no container with the
//! reserved name ... These belong with the live-podman work in 0002-53."
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test container_cancellation_e2e -- --nocapture
//! ```
//!
//! # Why each test cancels repeatedly, and why the two do it differently
//!
//! A cancel that landed *before* podman made anything would satisfy "no
//! container survives" while proving nothing, so each test cancels several
//! times and asserts that enough of them landed somewhere that counts. What
//! counts differs by path, because the two windows are not the same size.
//!
//! **`create` is arranged.** `podman create` returns, the engine holds a
//! container, and the future is still inside `podman init`. An earlier version
//! of this file went looking for that window with `podman ps`, and CI showed
//! what that costs: a poll spawns a process, and on a loaded runner the spawn
//! outlasts the window it is watching, so 0 of 5 cancels landed in flight and
//! the test failed having measured nothing -- three runs in a row, x86-64
//! only. The window is held open now rather than hunted for. `WRAPPER` parks
//! `podman init` after a real `create` has returned, so the engine provably
//! holds the container and the cancel lands there 5 times in 5 by
//! construction. This is the window 0002-39 named.
//!
//! **`start` is not.** `podman run -d` prints the container's id only once the
//! container is *up*, and `start_named` returns as soon as it has parsed that
//! id. The interval in which the engine holds the container and the future has
//! not yet returned is shorter than a `podman ps` takes, so polling for it
//! never wins: measured 0 times in 5. An earlier draft of this file appeared
//! to win it 8 times in 8, but only because it probed with a *blocking*
//! `std::process::Command` inside the `select!` -- which stopped the creation
//! future being polled for the duration of each probe, and so widened the very
//! window it was trying to land inside. The instrument was making the
//! measurement.
//!
//! So `start` cancels on a timer instead, swept across the span of a `podman
//! run`, and what it asserts is that the cancels landed while the call was
//! still in flight. It cannot also say the engine had already created the
//! container at that instant; `create` is what says that.
//!
//! Neither test can take its own mechanism away, the way
//! `build_cancellation_e2e.rs` can with `image::ungraceful_build_termination`
//! -- the container path has no counterpart -- so these counts are the
//! substitute for a negative control.
//!
//! Names are unique per process and per attempt, so nothing here reads engine
//! state it does not own, and the tests can run beside the rest of the suite.

#![cfg(feature = "e2e")]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use outrig::container::{self, Container, ContainerCreateOptions, ContainerLaunchSpec};
use outrig::image::ImageTag;

/// Serializes this binary's tests. Cargo runs test binaries in parallel, so
/// this is not a lock on the engine -- every assertion below is scoped to a
/// name this file minted, for exactly that reason.
static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long the engine gets to reach a state before a test gives up. Generous:
/// a loaded runner building images in a neighbouring test binary is the
/// ordinary case, not the exceptional one, and a cancelled *start* has to wait
/// out podman's stop grace besides -- see `await_engine_free`.
const ENGINE_CEILING: Duration = Duration::from_secs(60);

/// How many times each test cancels. `create` is arranged rather than raced,
/// so it lands 5 times out of 5 and asserts exactly that; `start` cancels on a
/// swept timer and has measured 5 in 5 as well. Five keeps a single attempt
/// from ever being the only evidence, and the file still finishes in seconds.
const ATTEMPTS: usize = 5;

/// Distinct from `build_cancellation_e2e.rs`'s `outrig-e2e-cancel-` prefix on
/// purpose. Both binaries run at once against one engine and both filter
/// engine listings by their own names; a shared prefix would leave that
/// separation resting on the pid alone.
fn unique_name(what: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    // `outrig-` so that a test which dies mid-flight leaves something
    // `outrig clean` will collect rather than an orphan only a human finds.
    format!(
        "outrig-e2e-container-cancel-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A `podman` that is the real one, except where a marker says to park.
///
/// `create_initialized` runs `podman create` and then `podman init <name>`, so
/// by the time `init` is spawned the engine is already holding the container.
/// This wrapper parks exactly there -- after a real create, before a real init
/// -- and publishes `holding.<name>`, which a test waits on with a stat rather
/// than with a subprocess. Every other invocation, including the cleanup's own
/// `podman rm`, execs the real engine, so a test that plants no marker behaves
/// as though this file were not here, and what the assertions read afterwards
/// is still real engine state.
const WRAPPER: &str = r#"#!/bin/sh
journal="$OUTRIG_E2E_JOURNAL"

# `podman init <name>`, and only for a name a test asked to hold. Publishing
# before parking is what lets the wait be a stat: the marker appearing means
# the real create has already returned and the engine holds the container.
if [ "$1" = "init" ] && [ -n "$2" ] && [ -f "$journal/hold.init.$2" ]; then
  : > "$journal/holding.$2"
  # `exec` so the cancellation's kill reaches the sleeper itself rather than a
  # shell that would have to forward it.
  exec sleep 300
fi

# Hand the search back to the shell, with this wrapper's directory removed.
# `execvp` is what decides which `podman` an ordinary spawn would have found:
# it skips a candidate the *effective* user cannot execute and keeps looking,
# which no mode-bit test here reproduces faithfully -- `mode & 0o111` is true
# of a file owned by this user at 0645, which it cannot execute. Rather than
# imitate that rule, use it.
PATH="$OUTRIG_REAL_PATH"
export PATH
exec podman "$@"
"#;

/// Install the wrapper ahead of the real podman on `PATH`, and return the
/// journal directory.
///
/// The same shape and the same synchronization as `cancellation.rs`'s
/// `fake_runtime`: `PATH` is process-global, so the write happens once inside
/// `OnceLock::get_or_init`, which blocks every other caller until the first has
/// returned. Every test in this binary calls this before it spawns anything.
fn wrapper_runtime() -> &'static Path {
    static JOURNAL: OnceLock<PathBuf> = OnceLock::new();
    JOURNAL.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;

        // Captured before `PATH` is rewritten: it is what the wrapper restores
        // so its own `exec podman` cannot find the wrapper again.
        let real_path = std::env::var_os("PATH").unwrap_or_default();

        let root = tempfile::Builder::new()
            .prefix("outrig-e2e-podman-wrapper")
            .tempdir()
            .expect("tempdir");
        let bin = root.path().join("bin");
        let journal = root.path().join("journal");
        std::fs::create_dir_all(&bin).expect("create wrapper bin dir");
        std::fs::create_dir_all(&journal).expect("create journal dir");

        let wrapper = bin.join("podman");
        std::fs::write(&wrapper, WRAPPER).expect("write wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("chmod wrapper");

        let mut search = std::ffi::OsString::from(&bin);
        search.push(":");
        search.push(std::env::var_os("PATH").unwrap_or_default());

        // SAFETY: edition 2024 marks `env::set_var` unsafe because of
        // multi-thread races. Both writes happen inside `get_or_init`, which
        // every test in this binary enters before spawning anything, so no
        // thread can read either variable concurrently with this write.
        unsafe {
            std::env::set_var("PATH", search);
            std::env::set_var("OUTRIG_E2E_JOURNAL", &journal);
            // Through the environment, never interpolated into the script: it
            // carries the original bytes, and the expansion of a quoted
            // variable is not rescanned, so an entry containing `$` or `"`
            // neither expands nor breaks the parse.
            std::env::set_var("OUTRIG_REAL_PATH", &real_path);
        }

        // The directory has to outlive every test in the binary, and nothing
        // runs after the last one, so it is left in the system temp dir rather
        // than removed. One small directory per `cargo test` run.
        std::mem::forget(root);
        journal
    })
}

/// Ask the wrapper to park `podman init <name>` when it reaches it.
fn hold_init_for(journal: &Path, name: &str) {
    std::fs::write(journal.join(format!("hold.init.{name}")), "").expect("write hold marker");
}

/// Resolves once the wrapper has parked `podman init <name>` -- so once the
/// real `podman create` has returned and the engine is holding the container.
///
/// A stat rather than a `podman ps`, which is the whole point of the wrapper:
/// the poll costs microseconds, so it cannot lose to the window it watches.
async fn await_holding(journal: &Path, name: &str) {
    let started = Instant::now();
    while !journal.join(format!("holding.{name}")).exists() {
        assert!(
            started.elapsed() < ENGINE_CEILING,
            "the wrapper never parked `podman init {name}` within {ENGINE_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Whether the engine currently holds a container under exactly `name`.
///
/// `tokio::process` rather than `common::run_capture`'s blocking
/// `std::process`, so that a neighbouring task keeps being polled while podman
/// answers. This used to run inside a `tokio::select!` arm racing the creation
/// future, where a blocking `output()` would have widened the very window the
/// select was trying to land inside; the wrapper above retired that use, and
/// only `await_engine_free` calls this now.
///
/// Exact-match rather than `--filter name=`, which podman treats as a regex: a
/// filter would answer for any container whose name merely contains this one,
/// and these names share a prefix by construction.
async fn engine_holds(name: &str) -> bool {
    let out = tokio::process::Command::new("podman")
        .args(["ps", "--all", "--format", "{{.Names}}"])
        .output()
        .await
        .unwrap_or_else(|e| panic!("podman ps --all: {e}"));
    assert!(
        out.status.success(),
        "podman ps --all failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == name)
}

/// Resolves once nothing under `name` is left, returning how long that took.
///
/// Polled far more loosely than `await_engine_holds`, because nothing asserts
/// on the figure -- it is reported, not checked -- and a `podman ps` costs more
/// than the interval would. Measured here: a cancel that landed before the
/// container was running frees the name in 25-500 ms, while one that landed
/// after a *start* had completed pays podman's full ten-second stop grace,
/// because the container runs `sleep infinity` as PID 1 and PID 1 discards a
/// SIGTERM it has no handler for. `plan/next/primary-image-needs-no-sleep.md`
/// owns that second figure. The ceiling is sized for it.
async fn await_engine_free(name: &str) -> Duration {
    let started = Instant::now();
    loop {
        if !engine_holds(name).await {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < ENGINE_CEILING,
            "a container named {name} outlived its cancellation by {ENGINE_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// When to take the creation future away.
enum CancelOn {
    /// Once the wrapper has parked `podman init` -- the strong form, and no
    /// longer a race: `podman create` has returned by then, so the engine is
    /// holding the container.
    EngineHolds,
    /// After a fixed delay, for a window too narrow to observe from outside.
    Elapsed(Duration),
}

/// Drive `creation` until either it finishes or `when` fires, then drop it and
/// wait the name back to free.
///
/// Returns whether the cancel landed with the creation still in flight. Under
/// `EngineHolds` that also means the engine was holding the container; under
/// `Elapsed` it means only that the call had not returned. The caller counts
/// them and says which it is asserting.
async fn cancel_once<F>(what: &str, name: &str, when: CancelOn, creation: F) -> bool
where
    F: Future<Output = outrig::error::Result<Container>>,
{
    let mut creation = Box::pin(creation);
    let completed = tokio::select! {
        outcome = &mut creation => Some(outcome),
        () = async {
            match when {
                CancelOn::EngineHolds => {
                    await_holding(wrapper_runtime(), name).await;
                    // The premise, now that it costs nothing to check. While
                    // the wrapper parks, the window stays open for as long as
                    // this takes -- so the `podman ps` that used to be the
                    // race is just a read, and the test can prove it really is
                    // cancelling against a container the engine holds rather
                    // than against nothing.
                    assert!(
                        engine_holds(name).await,
                        "the wrapper parked `podman init {name}`, but the engine holds no \
                         container under that name"
                    );
                }
                CancelOn::Elapsed(delay) => tokio::time::sleep(delay).await,
            }
        } => None,
    };
    drop(creation);

    // A creation that won the race hands back a `Container`, and dropping it
    // is what removes it -- the same invariant by the other door, so the wait
    // below is meaningful either way. An `Err` is neither: it is the engine
    // refusing, and counting it as "the cancel arrived late" is how a podman
    // that fails on every attempt used to surface as `require_in_flight`'s
    // "the window may have moved" instead of as its own error.
    let in_flight = match completed {
        None => true,
        Some(Ok(container)) => {
            drop(container);
            false
        }
        Some(Err(e)) => panic!("{what}: the engine refused to create {name}: {e}"),
    };

    let freed_in = await_engine_free(name).await;
    println!(
        "{what}: cancel landed {}; engine free in {freed_in:?}",
        if in_flight {
            "with the creation still in flight"
        } else {
            "after the creation had returned"
        }
    );
    in_flight
}

/// Assert the run landed inside the window often enough to have measured
/// anything.
fn require_in_flight(what: &str, in_flight: usize, window: &str) {
    assert!(
        in_flight > 0,
        "none of the {ATTEMPTS} {what} cancels landed {window}, so nothing was measured -- \
         the window may have moved, or podman may be failing before it creates anything"
    );
    println!("{what}: {in_flight}/{ATTEMPTS} cancels landed {window}");
}

/// The arranged form of the above: every attempt must land in the window,
/// because nothing about where it lands is left to timing.
///
/// A shortfall is a broken instrument rather than a slow engine -- a `podman
/// init` that moved, or a marker the wrapper never read -- and saying so is the
/// point of asserting the stronger thing. Polling for the window could only
/// ever justify "at least one", which is what let CI report 0 of 5.
fn require_every_attempt_in_flight(what: &str, in_flight: usize) {
    assert_eq!(
        in_flight, ATTEMPTS,
        "{what}: {in_flight} of {ATTEMPTS} cancels landed with the engine holding the container, \
         so the wrapper is no longer parking `podman init` where this test needs it"
    );
    println!("{what}: {in_flight}/{ATTEMPTS} cancels landed with the engine holding the container");
}

/// A cancelled `podman run` leaves the engine with no container under the
/// reserved name, and releases the name in-process too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_start_leaves_no_container() {
    let _serialized = E2E_LOCK.lock().await;
    common::init_tracing();
    // Before `pull_alpine`, and before anything else in this binary spawns a
    // process: `wrapper_runtime` rewrites `PATH`, and that write is only sound
    // because every test reaches it first.
    wrapper_runtime();
    common::pull_alpine();

    let image = ImageTag::new(common::ALPINE);
    let mut in_flight = 0usize;
    for attempt in 0..ATTEMPTS {
        // Swept across the span of a `podman run` rather than fixed, so the
        // cancels land at different points in it and no single one of them
        // decides what the test measures.
        let delay = Duration::from_millis(20 + 40 * attempt as u64);
        let name = unique_name("start");
        let landed = cancel_once(
            "start",
            &name,
            CancelOn::Elapsed(delay),
            Container::start_named(&image, ContainerLaunchSpec::default(), name.clone(), None),
        )
        .await;
        assert!(
            !container::is_tracked(&name),
            "{name} is still tracked in-process after its creation was cancelled"
        );
        in_flight += usize::from(landed);
    }
    require_in_flight("start", in_flight, "while the call was still in flight");
}

/// The same for `create` -> `init`, the boundary with the most to lose: once
/// `podman create` has returned, the engine holds a container while the future
/// is still parked inside `podman init`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_create_leaves_no_container() {
    let _serialized = E2E_LOCK.lock().await;
    common::init_tracing();
    // Before `pull_alpine`, and before anything else in this binary spawns a
    // process: `wrapper_runtime` rewrites `PATH`, and that write is only sound
    // because every test reaches it first.
    wrapper_runtime();
    common::pull_alpine();

    let mut in_flight = 0usize;
    for _ in 0..ATTEMPTS {
        let name = unique_name("create");
        // Planted before the creation starts: the wrapper reads it when
        // `create_initialized` reaches `podman init`, which is after the real
        // `podman create` has returned.
        hold_init_for(wrapper_runtime(), &name);
        let options = ContainerCreateOptions::new(
            ImageTag::new(common::ALPINE),
            ContainerLaunchSpec::default(),
            &name,
        );
        let landed = cancel_once(
            "create",
            &name,
            CancelOn::EngineHolds,
            Container::create_initialized(options),
        )
        .await;
        assert!(
            !container::is_tracked(&name),
            "{name} is still tracked in-process after its creation was cancelled"
        );
        in_flight += usize::from(landed);
    }
    require_every_attempt_in_flight("create", in_flight);
}
