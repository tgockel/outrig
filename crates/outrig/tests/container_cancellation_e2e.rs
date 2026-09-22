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
//! **`create` is observable.** `podman create` returns, the engine holds a
//! container, and the future is still inside `podman init`. That window is
//! hundreds of milliseconds wide -- comfortably wider than the `podman ps`
//! that observes it -- so the test waits until the engine demonstrably holds
//! the container and cancels then. This is the window 0002-39 named.
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

/// How many times each test cancels. Measured at five: `start` lands in flight
/// 5 times out of 5, and `create` catches the engine holding the container 4
/// times out of 5, so one run is never the only evidence and the file still
/// finishes in seconds.
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

/// Whether the engine currently holds a container under exactly `name`.
///
/// `tokio::process` rather than `common::run_capture`'s blocking
/// `std::process`: this runs inside a `tokio::select!` arm racing the creation
/// future, and a blocking `output()` there stops that future being polled for
/// as long as podman takes -- which would widen the very window the select is
/// trying to land inside.
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

/// Resolves once the engine is demonstrably holding `name`. Polled tightly,
/// because how soon this resolves is what decides whether the cancel lands
/// inside the window. The ceiling is for the case where *neither* this nor the
/// creation it is raced against resolves, which is a hang worth failing on.
async fn await_engine_holds(name: &str) {
    let started = Instant::now();
    while !engine_holds(name).await {
        assert!(
            started.elapsed() < ENGINE_CEILING,
            "the engine never held {name} within {ENGINE_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
    /// Once the engine is observably holding the container -- the strong form,
    /// usable where the window is wider than a `podman ps`.
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
                CancelOn::EngineHolds => await_engine_holds(name).await,
                CancelOn::Elapsed(delay) => tokio::time::sleep(delay).await,
            }
        } => None,
    };
    drop(creation);

    let in_flight = completed.is_none();
    // A creation that won the race hands back a `Container`, and dropping it
    // is what removes it -- the same invariant by the other door, so the wait
    // below is meaningful either way.
    drop(completed);

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

/// A cancelled `podman run` leaves the engine with no container under the
/// reserved name, and releases the name in-process too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_start_leaves_no_container() {
    let _serialized = E2E_LOCK.lock().await;
    common::init_tracing();
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
    common::pull_alpine();

    let mut in_flight = 0usize;
    for _ in 0..ATTEMPTS {
        let name = unique_name("create");
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
    require_in_flight("create", in_flight, "with the engine holding the container");
}
