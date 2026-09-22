//! What a cancelled `buildah build` leaves in a real engine.
//!
//! Gated behind `--features e2e` because only a live buildah can answer the
//! question. The fakes in `tests/cancellation.rs` prove outrig asks its build
//! to stop; nothing short of an engine proves buildah then removed the
//! per-stage working containers it had made, which is the whole claim.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test build_cancellation_e2e -- --nocapture
//! ```
//!
//! Expect a minute or two: the cancellations are real, and
//! `twenty_cancellations_leave_no_growth` is twenty of them.
//!
//! # Why every assertion is a delta
//!
//! `E2E_LOCK` serializes the tests in *this* binary, and cargo runs test
//! binaries in parallel, so the absolute contents of `buildah containers` are
//! never this file's to predict. Each test therefore builds its own uniquely
//! named base image and asserts only about working containers derived from
//! it -- buildah names a stage container after the image it came from, so
//! that marker is carried by exactly the containers this test provoked.

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use outrig::image::{self, ImageTag};

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a live engine gets to reach a state before the test gives up.
/// Generous: these wait on real builds and real cleanups.
const ENGINE_CEILING: Duration = Duration::from_secs(120);

/// A `RUN` long enough that the cancellation lands well inside it. buildah
/// registers its signal handler only while a `RUN`'s child is executing, so a
/// test that drops the future near the edge of that window would be measuring
/// the gap rather than the mechanism.
const RUN_SECONDS: u32 = 30;

/// How long to let the `RUN` get under way before cancelling.
///
/// The working container appears *before* the handler does -- it is created
/// to run the step in -- so cancelling the instant it shows up lands in the
/// gap where buildah has no handler registered and dies where it stands.
/// Measured: doing exactly that leaked on roughly one cancellation in three.
///
/// A fixed wait is sound here in a way it would not be for an assertion. It
/// chooses *when* to cancel, not what to conclude, and every instant it can
/// choose is inside a `RUN` that lasts `RUN_SECONDS`. The gap it steps over
/// is real and is what `outrig clean --build-containers` exists to collect;
/// it is documented rather than asserted away.
const RUN_SETTLE: Duration = Duration::from_secs(3);

/// One buildah working container, as `buildah containers --json` reports it.
#[derive(Debug, Clone, serde::Deserialize)]
struct WorkingContainer {
    id: String,
    #[serde(default)]
    containername: String,
    #[serde(default)]
    imagename: String,
}

/// A marker no other test, binary, or run will use, which every image and
/// container this test provokes carries somewhere in its name.
fn marker(what: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "outrig-e2e-cancel-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn buildah(args: &[&str]) -> std::process::Output {
    std::process::Command::new("buildah")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("buildah {args:?}: {e}"))
}

/// Every buildah working container currently in the store.
///
/// `--json` prints `null` rather than `[]` for an empty store on some
/// versions, so both are read as "none".
fn working_containers() -> Vec<WorkingContainer> {
    let out = buildah(&["containers", "--json"]);
    assert!(
        out.status.success(),
        "buildah containers --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Option<Vec<WorkingContainer>>>(&out.stdout)
        .unwrap_or_else(|e| {
            panic!(
                "parsing buildah containers --json: {e}; output was {:?}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
        .unwrap_or_default()
}

/// The working containers this test provoked: buildah names a stage container
/// after the image it was created from, so the base image's marker is in both
/// the container name and the image name.
fn provoked(marker: &str) -> Vec<WorkingContainer> {
    working_containers()
        .into_iter()
        .filter(|c| c.containername.contains(marker) || c.imagename.contains(marker))
        .collect()
}

/// Wait until the build has created a stage working container, which is proof
/// that it is inside the stage rather than still resolving its base.
async fn await_working_container(marker: &str) -> WorkingContainer {
    let deadline = Instant::now() + ENGINE_CEILING;
    loop {
        if let Some(found) = provoked(marker).into_iter().next() {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no working container derived from {marker} appeared within {ENGINE_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until nothing derived from `marker` is left, returning how long it
/// took. That figure is an assertion in its own right: a buildah that
/// absorbed the stop and ran to completion would also end clean, just far
/// later, and the difference is the whole point of asking it to stop.
async fn await_no_working_container(marker: &str) -> Duration {
    let started = Instant::now();
    loop {
        let left = provoked(marker);
        if left.is_empty() {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < ENGINE_CEILING,
            "working containers derived from {marker} outlived {ENGINE_CEILING:?}: {left:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Remove anything this test provoked, by id. Best-effort: a test that has
/// already failed should still leave the machine usable.
fn sweep(marker: &str) {
    for container in provoked(marker) {
        let _ = buildah(&["rm", &container.id]);
    }
    for image in images_named(marker) {
        let _ = buildah(&["rmi", "--force", &image]);
    }
}

/// Every image in the store whose name carries `marker`.
fn images_named(marker: &str) -> Vec<String> {
    let out = buildah(&["images", "--format", "{{.Name}}:{{.Tag}}", "--noheading"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| line.contains(marker))
        .map(str::to_string)
        .collect()
}

/// Build a base image this test owns, so every stage container derived from
/// it is identifiable as this test's doing.
fn build_base(marker: &str) -> ImageTag {
    let tag = ImageTag::new(format!("{marker}:base"));
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Dockerfile"),
        "FROM docker.io/library/alpine:latest\nRUN true\n",
    )
    .expect("write base Dockerfile");
    let out = buildah(&[
        "build",
        "--tag",
        tag.as_str(),
        "--file",
        dir.path().join("Dockerfile").to_str().expect("utf-8 path"),
        dir.path().to_str().expect("utf-8 path"),
    ]);
    assert!(
        out.status.success(),
        "building the base image failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tag
}

/// A two-stage context whose first stage parks in a long `RUN`.
fn slow_context(base: &ImageTag) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Dockerfile"),
        format!(
            "FROM {base} AS slow\nRUN sleep {RUN_SECONDS}\n\nFROM {base}\nCOPY --from=slow / /\n"
        ),
    )
    .expect("write Dockerfile");
    dir
}

fn final_tag(marker: &str) -> ImageTag {
    ImageTag::new(format!("{marker}:built"))
}

/// Start `build`, cancel it once it is provably inside a stage, and return
/// how long the engine took to come back to where it started.
async fn cancel_mid_stage<F: std::future::Future>(marker: &str, build: F) -> Duration {
    let mut build = Box::pin(build);
    await_run_under_way(marker, &mut build).await;
    drop(build);
    await_no_working_container(marker).await
}

/// Drive `build` until its stage is provably running the step, not merely
/// holding a container to run it in. See [`RUN_SETTLE`].
async fn await_run_under_way<F: std::future::Future>(
    marker: &str,
    build: &mut std::pin::Pin<Box<F>>,
) {
    tokio::select! {
        _ = &mut *build => panic!("the build should not have finished on its own"),
        _ = await_working_container(marker) => {}
    }
    tokio::select! {
        _ = &mut *build => panic!("the build should not have finished on its own"),
        _ = tokio::time::sleep(RUN_SETTLE) => {}
    }
}

/// The measurement this task exists for: a two-stage build cancelled inside a
/// `RUN` leaves `buildah containers` as it found it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_multi_stage_build_leaves_no_working_container() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("multistage");
    let base = build_base(&marker);
    let ctx = slow_context(&base);

    let cfg = common::fixture_build_config();
    let took = cancel_mid_stage(
        &marker,
        image::build_image_for(&marker, &cfg, ctx.path(), &final_tag(&marker), true),
    )
    .await;

    // buildah cleaning up its own stage containers is a teardown, not a
    // build: if this took anything like `RUN_SECONDS` the stop was absorbed
    // and the build simply ran to completion, which is clean for the wrong
    // reason and much slower than cancelling ought to be.
    assert!(
        took < Duration::from_secs(RUN_SECONDS.into()),
        "the engine took {took:?} to come clean, which is the build finishing rather than stopping"
    );
    println!("canceled multi-stage build: engine clean in {took:?}");
    sweep(&marker);
}

/// The transcript-logged path reaches buildah through a different helper, so
/// it gets its own measurement rather than inheriting one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_logged_build_leaves_no_working_container() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("logged");
    let base = build_base(&marker);
    let ctx = slow_context(&base);
    let transcript = outrig::Transcript::create(&ctx.path().join("transcript.log"), false)
        .await
        .expect("create transcript");

    let cfg = common::fixture_build_config();
    let took = cancel_mid_stage(
        &marker,
        image::ensure_tagged_image_for(
            &marker,
            &cfg,
            ctx.path(),
            &final_tag(&marker),
            true,
            Some(&transcript),
        ),
    )
    .await;

    println!("canceled logged build: engine clean in {took:?}");
    sweep(&marker);
}

/// And the standalone path, which armed nothing at all before this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_standalone_build_leaves_no_working_container() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("standalone");
    let base = build_base(&marker);
    let ctx = slow_context(&base);

    let labels = BTreeMap::new();
    let took = cancel_mid_stage(
        &marker,
        image::build_standalone(
            ctx.path(),
            Path::new("Dockerfile"),
            Path::new("."),
            &final_tag(&marker),
            true,
            &labels,
        ),
    )
    .await;

    println!("canceled standalone build: engine clean in {took:?}");
    assert!(
        !images_named(&marker)
            .iter()
            .any(|name| name.contains("outrig-tmp-")),
        "a cancelled standalone build left its temporary tag: {:?}",
        images_named(&marker)
    );
    sweep(&marker);
}

/// Cancelling repeatedly accumulates nothing. The leak this task fixes was
/// per-cancellation, so one is a measurement and twenty is the claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_cancellations_leave_no_growth() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("repeat");
    let base = build_base(&marker);
    let ctx = slow_context(&base);
    let before = images_named(&marker);

    let cfg = common::fixture_build_config();
    for attempt in 0..20 {
        let took = cancel_mid_stage(
            &marker,
            image::build_image_for(&marker, &cfg, ctx.path(), &final_tag(&marker), true),
        )
        .await;
        println!("cancellation {attempt}: engine clean in {took:?}");
    }

    assert!(
        provoked(&marker).is_empty(),
        "twenty cancellations left working containers: {:?}",
        provoked(&marker)
    );
    // A completed build would leave its final tag; a cancelled one leaves
    // neither that nor the temporary tag it built into.
    assert_eq!(
        images_named(&marker),
        before,
        "twenty cancellations changed the image store"
    );
    sweep(&marker);
}

/// The case a name-based cleanup gets wrong, and the one this task exists to
/// get right: two builds from one base contend for the same
/// `<base>-working-container` name, and cancelling one must not reach the
/// other. Asserted on the surviving container's **id**, which is the only
/// identity a name cannot be mistaken for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_build_from_the_same_base_is_untouched() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("concurrent");
    let base = build_base(&marker);
    let keep_ctx = slow_context(&base);
    let cancel_ctx = slow_context(&base);

    let cfg = common::fixture_build_config();
    let keep_tag = ImageTag::new(format!("{marker}:keep"));
    let mut keep = Box::pin(image::build_image_for(
        &marker,
        &cfg,
        keep_ctx.path(),
        &keep_tag,
        true,
    ));

    // Let the survivor reach its stage first, and record what it made.
    let survivor = tokio::select! {
        _ = &mut keep => panic!("the surviving build should not have finished yet"),
        found = await_working_container(&marker) => found,
    };
    tokio::select! {
        _ = &mut keep => panic!("the surviving build should not have finished yet"),
        _ = tokio::time::sleep(RUN_SETTLE) => {}
    }

    let cancel_tag = ImageTag::new(format!("{marker}:canceled"));
    let mut canceled = Box::pin(image::build_image_for(
        &marker,
        &cfg,
        cancel_ctx.path(),
        &cancel_tag,
        true,
    ));
    // Wait for a *second* container, so the cancellation lands while both
    // builds hold one.
    let deadline = Instant::now() + ENGINE_CEILING;
    loop {
        let held = tokio::select! {
            _ = &mut keep => panic!("the surviving build should not have finished yet"),
            _ = &mut canceled => panic!("the build under cancellation should not have finished"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => provoked(&marker),
        };
        if held.len() >= 2 {
            // Both contenders hold a container; let the second one reach its
            // `RUN` before the cancellation lands, for RUN_SETTLE's reason.
            tokio::select! {
                _ = &mut keep => panic!("the surviving build should not have finished yet"),
                _ = &mut canceled => panic!("the build under cancellation should not have finished"),
                _ = tokio::time::sleep(RUN_SETTLE) => {}
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the second build never took a working container; held: {held:?}"
        );
    }
    drop(canceled);

    // The survivor's container is still there, by id, and its build still
    // reaches its own ending. One snapshot per tick, so the claim is about a
    // single observed state rather than two taken 100 ms apart.
    let deadline = Instant::now() + ENGINE_CEILING;
    let left = loop {
        let held = provoked(&marker);
        if held.len() <= 1 {
            break held;
        }
        assert!(
            Instant::now() < deadline,
            "the cancelled build's container outlived {ENGINE_CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        left.iter().any(|c| c.id == survivor.id),
        "cancelling one build removed the other's container {}",
        survivor.id
    );
    keep.await.expect("the surviving build must still succeed");

    sweep(&marker);
}

/// The negative control. Take the graceful stop away and the leak comes back
/// -- so the passing assertions above are the mechanism working, not the
/// engine having been clean anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_graceful_stop_is_what_keeps_the_engine_clean() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();
    let marker = marker("control");
    let base = build_base(&marker);
    let ctx = slow_context(&base);

    let cfg = common::fixture_build_config();
    let tag = final_tag(&marker);
    // Scoped to this build, so nothing else in the binary can see it and
    // there is nothing to reset if this test panics.
    let mut build = Box::pin(image::ungraceful_build_termination(image::build_image_for(
        &marker,
        &cfg,
        ctx.path(),
        &tag,
        true,
    )));
    await_run_under_way(&marker, &mut build).await;
    drop(build);

    // Nothing will collect this, so a fixed wait is sound here in a way it
    // would not be for an assertion of absence: the state under test is the
    // one that persists.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let left = provoked(&marker);
    assert!(
        !left.is_empty(),
        "killing buildah outright left no working container, so the graceful \
         stop in the tests above was not what made them pass"
    );
    println!("negative control: killing buildah outright left {left:?}");
    sweep(&marker);
}
