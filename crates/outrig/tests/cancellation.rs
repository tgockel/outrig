//! Cancellation ownership at the container and image layers.
//!
//! Deliberately **not** `e2e`-gated. Podman and buildah are faked with shell
//! scripts, so these run in the ordinary suite -- the same reasoning as
//! `outrig-cli/tests/builtin_default.rs`: a guarantee that only CI's unrunnable
//! row can check is a guarantee nobody checks.
//!
//! What the fakes can prove is that outrig kills the client it spawned and
//! issues the engine-side cleanup it owes. What they cannot prove is that the
//! *engine* then has no container, no working container, and no stray tag;
//! that needs a real podman and belongs with the live-engine work in 0129.
//!
//! Process-level ownership (every capture shape, the spawn-to-owner handoff,
//! the cooperative confirmed reap) is covered in `src/process_tests.rs`, which
//! needs no fake at all.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use outrig::container::{Container, ContainerCreateOptions, ContainerLaunchSpec};
use outrig::image::ImageTag;

/// Generous ceiling for anything this file polls for. The real figures are
/// milliseconds; this is margin for a loaded runner.
const CEILING: Duration = Duration::from_secs(10);

/// A `podman` / `buildah` stand-in that journals every invocation and can be
/// steered per subject.
///
/// The journal is one file per invocation holding the caller's pid and its
/// argv, rather than shared appends to one file: several fakes run at once
/// (the test's own client, plus whatever cleanup outrig detaches), and a
/// per-invocation file cannot interleave with another.
///
/// An invocation becomes *visible* to a test only once acting on it is safe,
/// which is later than when it starts. A test cancels on first sight of an
/// invocation, and for a creating verb the container has to exist -- carrying
/// the attempt label outrig's cleanup filters on -- before that sight, or the
/// kill can beat the fake to recording the label and the label-scoped removal
/// then races the creation it is undoing. So each invocation is staged under a
/// name the test does not look for and renamed into place at its own safe
/// point.
///
/// `run` and `create` record the attempt label they were given, and a removal
/// filtered on a label matches only what carries it -- which is how outrig's
/// cleanup scopes itself to the container it asked for.
///
/// `run`, `create`, `init`, `start`, `build` and `from` sleep instead of
/// returning, so a test can cancel while one is in flight. A `fast.<verb>.<token>` marker
/// makes that one verb return immediately when `<token>` appears in its argv,
/// which is how a test reaches the *boundary between* two commands instead of
/// the middle of the first. Scoping the marker to a verb matters: `podman
/// create --name N` and `podman init N` both mention `N`, so an unscoped
/// marker would make the whole sequence fast and the test would prove nothing.
const FAKE: &str = r#"#!/bin/sh
journal="$OUTRIG_FAKE_JOURNAL"
pending=$(mktemp "$journal/pending.XXXXXX")
printf '%s\n%s\n' "$$" "$*" > "$pending"

# Publish this invocation: `inv.` is the prefix the test polls for, and nothing
# may carry it before whatever a cancellation triggered by that sighting is
# entitled to find. rename(2) is atomic, so a published record is a complete
# one. Every exit path below publishes exactly once.
publish() {
  mv "$pending" "$journal/inv.${pending##*/pending.}"
}

# Enough of a real answer for the label read between build and commit; every
# other verb only has to succeed.
if [ "$1 $2" = "image inspect" ]; then
  publish
  printf '{}\n'
  exit 0
fi

# Which container this invocation is about, the per-attempt label it was told
# to stamp on what it creates, and what a removal was scoped to.
subject=
label=
filter=
prev=
for arg in "$@"; do
  case "$prev" in
    --name) subject=$arg ;;
    --label) label=$arg ;;
    --filter) filter=$arg ;;
  esac
  prev=$arg
done

# Removals record *what they removed*, so a test can assert on the container
# rather than on the shape of an argv. A label-scoped removal reaches only
# what carries that label; a bare one names its target directly.
case "$1" in
  rm|rmi)
    if [ -n "$filter" ]; then
      lbl=${filter#label=}
      if [ -f "$journal/labeled.$lbl" ]; then
        removed=$(cat "$journal/labeled.$lbl")
        : > "$journal/removed.$(printf '%s' "$removed" | tr '/:' '__')"
      fi
    else
      for arg in "$@"; do target=$arg; done
      : > "$journal/removed.$(printf '%s' "$target" | tr '/:' '__')"
    fi
    publish
    exit 0
    ;;
esac

case "$1" in
  run|create|init|start|build|from)
    # A `hold.<verb>.<token>` marker parks the client *before* it creates
    # anything, and says so: a test can then cancel one that is provably alive
    # with nothing behind it. Two causes look identical from outrig's side --
    # the name belongs to somebody else, or podman has simply not got there
    # yet -- and one mechanism serves both, because outrig cannot tell them
    # apart either. Nothing here writes a `labeled.` file, so a removal scoped
    # to the attempt label reaches nothing, which is the point.
    for marker in "$journal"/hold."$1".*; do
      [ -e "$marker" ] || continue
      token=${marker##*/hold.$1.}
      case " $* " in
        *"$token"*)
          : > "$journal/holding.$token"
          publish
          exec sleep 30
          ;;
      esac
    done
    # A `fail.<verb>.<token>` marker is podman's name-collision failure: the
    # name belongs to another container, so nothing is created and no id is
    # recorded. This is the case a name-based cleanup would get wrong.
    for marker in "$journal"/fail."$1".*; do
      [ -e "$marker" ] || continue
      token=${marker##*/fail.$1.}
      case " $* " in
        *"$token"*)
          echo "Error: the container name is already in use" 1>&2
          publish
          exit 125
          ;;
      esac
    done
    # Past here the container exists, carrying the label it was created with.
    # A collision above never gets here, so nothing bearing that label exists.
    # The publish follows the label, never precedes it: that order is what
    # makes a cancellation on first sight of this invocation find a container
    # to remove rather than race its creation.
    if [ -n "$label" ]; then
      printf '%s\n' "$subject" > "$journal/labeled.$label"
    fi
    publish
    for marker in "$journal"/fast."$1".*; do
      [ -e "$marker" ] || continue
      token=${marker##*/fast.$1.}
      case " $* " in
        *"$token"*) exit 0 ;;
      esac
    done
    # `exec` so the pid recorded above is the process that is actually
    # asleep: a forked `sleep` would survive the kill and leak into the
    # developer's session. 30s is comfortably longer than any ceiling here, so
    # a sleeper never exits on its own and lets an assertion pass for the wrong
    # reason -- and short enough that a failing run cleans up after itself.
    exec sleep 30
    ;;
esac
publish
exit 0
"#;

/// Install the fakes and return the journal directory.
///
/// `PATH` is process-global, and this repo's convention for env mutation in
/// tests -- a variable name unique to each test -- cannot apply to a name
/// every process already reads. The synchronization instead comes from
/// `OnceLock::get_or_init`, which blocks every other caller until the first
/// has returned: every test in this binary calls this before it does anything
/// else, so each one's spawns happen-after the single write.
fn fake_runtime() -> &'static Path {
    static JOURNAL: OnceLock<PathBuf> = OnceLock::new();
    JOURNAL.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("outrig-fake-runtime")
            .tempdir()
            .expect("tempdir");
        let bin = root.path().join("bin");
        let journal = root.path().join("journal");
        std::fs::create_dir_all(&bin).expect("create fake bin dir");
        std::fs::create_dir_all(&journal).expect("create journal dir");

        for name in ["podman", "buildah"] {
            let path = bin.join(name);
            std::fs::write(&path, FAKE).expect("write fake");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake");
        }

        let mut path = std::ffi::OsString::from(&bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());

        // SAFETY: edition 2024 marks `env::set_var` unsafe because of
        // multi-thread races. These two writes happen inside `get_or_init`,
        // which every test in this binary enters before spawning anything, so
        // no thread can read either variable concurrently with this write.
        unsafe {
            std::env::set_var("PATH", path);
            std::env::set_var("OUTRIG_FAKE_JOURNAL", &journal);
        }

        // The directory has to outlive every test in the binary, and nothing
        // runs after the last one, so it is left in the system temp dir rather
        // than removed. One small directory per `cargo test` run.
        std::mem::forget(root);
        journal
    })
}

/// A name no other test in this binary will use, and which the fake can be
/// steered by.
fn unique_name(what: &str) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "outrig-cancel-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Make the fake's `verb` return immediately, rather than sleeping, whenever
/// `token` appears in its argv.
fn return_immediately_for(journal: &Path, verb: &str, token: &str) {
    std::fs::write(journal.join(format!("fast.{verb}.{token}")), "").expect("write fast marker");
}

/// Make the fake's `verb` fail the way podman fails a name collision: nothing
/// created, so nothing carrying the attempt label.
fn fail_name_collision_for(journal: &Path, verb: &str, token: &str) {
    std::fs::write(journal.join(format!("fail.{verb}.{token}")), "").expect("write fail marker");
}

/// Park the fake's `verb` before it creates anything and keep it alive there,
/// so a test can cancel a client that provably has no container behind it.
///
/// Both ways a real client reaches that state share this one marker: the name
/// belongs to someone else, or creation has not happened yet. Outrig never
/// learns why a command ended, so a fake that told them apart would be
/// modelling information outrig does not have.
fn hold_before_creating_for(journal: &Path, verb: &str, token: &str) {
    std::fs::write(journal.join(format!("hold.{verb}.{token}")), "").expect("write hold marker");
}

/// Whether the fake has reached that hold for `token`.
fn holding(journal: &Path, token: &str) -> bool {
    journal.join(format!("holding.{token}")).exists()
}

/// Drive `start` until the fake is provably holding for `token`, asserting it
/// stays pending throughout.
///
/// A single poll would prove nothing: it can stop at the SELinux probe before
/// podman is even spawned, or run all the way to the ordinary error -- and
/// either would let a caller pass without ever testing a cancellation.
async fn drive_until_holding<F: Future>(journal: &Path, start: &mut Pin<Box<F>>, token: &str) {
    let deadline = Instant::now() + CEILING;
    loop {
        assert!(
            futures_util::poll!(&mut *start).is_pending(),
            "the held client must not let the call finish"
        );
        if holding(journal, token) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the fake never reached the hold for {token:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Wait for a removal filtered on `attempt_label`, which is the guard doing
/// its job whether or not anything carries that label.
async fn expect_removal_scoped_to(journal: &Path, attempt_label: &str) {
    let filter = format!("label={attempt_label}");
    let deadline = Instant::now() + CEILING;
    while !removals(journal).iter().any(|argv| argv.contains(&filter)) {
        assert!(
            Instant::now() < deadline,
            "no removal was scoped to {filter:?} within {CEILING:?}; \
             removals seen: {:?}",
            removals(journal)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Whether the fake has removed the container or tag named `subject`.
///
/// This is what the cleanup assertions turn on, rather than the shape of a
/// removal's argv: a label-scoped removal reaches only what carries the label,
/// so "which container went away" is the question worth asking.
fn was_removed(journal: &Path, subject: &str) -> bool {
    // An image tag carries `/` and `:`, which a file name cannot; the fake
    // folds both the same way.
    let flat = subject.replace(['/', ':'], "_");
    journal.join(format!("removed.{flat}")).exists()
}

/// Wait until `subject` has been removed.
///
/// The failure reports the removals the fake saw, since "no removal was
/// issued" and "one was issued but scoped to something this subject does not
/// carry" are different defects and the bare assertion tells them apart.
async fn expect_removed(journal: &Path, subject: &str) {
    let deadline = Instant::now() + CEILING;
    while !was_removed(journal, subject) {
        assert!(
            Instant::now() < deadline,
            "{subject:?} was never removed, within {CEILING:?}; \
             removals seen: {:?}",
            removals(journal)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Assert `subject` is still there, once this attempt's cleanup has provably
/// run.
///
/// The wait is the assertion's foundation, not politeness: a negative is only
/// worth something after the removal that was going to happen has happened. A
/// fixed delay cannot give that -- a regression that reached for the container
/// by name could be held off by scheduling or a cold engine and land just
/// after the sleep expired, destroying the very container this is protecting
/// while the test reported success.
///
/// Either shape ends the wait, deliberately. Requiring the *correct* one would
/// make this assert the guard's scoping, which is
/// [`expect_removal_scoped_to`]'s job; here the only question is whether the
/// cleanup has run, so that "nothing was removed" is a statement about a
/// finished action. The fake publishes a removal's invocation only after
/// recording what it removed, so seeing the invocation means any mark it would
/// have left is already on disk.
async fn expect_not_removed(journal: &Path, attempt_label: &str, subject: &str) {
    let filter = format!("label={attempt_label}");
    let deadline = Instant::now() + CEILING;
    while !removals(journal)
        .iter()
        .any(|argv| argv.contains(&filter) || argv.contains(subject))
    {
        assert!(
            Instant::now() < deadline,
            "no cleanup for {subject:?} ran within {CEILING:?}, so \"nothing \
             was removed\" would assert nothing; removals seen: {:?}",
            removals(journal)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !was_removed(journal, subject),
        "{subject:?} was removed; nothing may remove a container this call \
         did not create"
    );
}

/// The attempt label podman was told to stamp, read back off the invocation
/// outrig actually issued rather than reconstructed here.
///
/// The launch specs in this file carry no labels of their own, so the attempt
/// label is the only `--label` on the argv.
async fn attempt_label_of(journal: &Path, verb: &str, name: &str) -> String {
    let (_, argv) = invocation(journal, verb, &[name]).await;
    let attempt = flag_value(&argv, "--label");
    assert!(
        attempt.starts_with("org.outrig.attempt="),
        "the start should stamp its attempt label, got {attempt:?}"
    );
    attempt
}

/// Every journalled invocation, as `(pid, argv)`.
fn invocations(journal: &Path) -> Vec<(u32, String)> {
    let Ok(entries) = std::fs::read_dir(journal) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("inv."))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| {
            let (pid, argv) = text.split_once('\n')?;
            Some((pid.trim().parse().ok()?, argv.trim().to_string()))
        })
        .collect()
}

/// Every removal the fake was asked for, as an argv.
fn removals(journal: &Path) -> Vec<String> {
    invocations(journal)
        .into_iter()
        .map(|(_, argv)| argv)
        .filter(|argv| verb_of(argv) == "rm" || verb_of(argv) == "rmi")
        .collect()
}

/// The subcommand an invocation is, which is its first word.
fn verb_of(argv: &str) -> &str {
    argv.split_whitespace().next().unwrap_or_default()
}

/// Wait for an invocation of `verb` whose argv contains every fragment, and
/// return it. Async, and sleeping through tokio rather than the thread, so
/// that the runtime keeps driving whatever this is waiting on.
///
/// The verb is the invocation's first word rather than one more fragment, for
/// the reason the `fast.<verb>.<token>` markers are verb-scoped: a container
/// name embeds the word a test steers on -- `outrig-cancel-init-...` contains
/// `init` -- so a fragment match on the verb finds `create --name
/// outrig-cancel-init-...` and the test cancels a phase earlier than it says.
async fn invocation(journal: &Path, verb: &str, fragments: &[&str]) -> (u32, String) {
    let deadline = Instant::now() + CEILING;
    loop {
        if let Some(found) = invocations(journal)
            .into_iter()
            .find(|(_, argv)| verb_of(argv) == verb && fragments.iter().all(|f| argv.contains(f)))
        {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no {verb} invocation matched {fragments:?} within {CEILING:?}; saw {:?}",
            invocations(journal)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn invocation_pid(journal: &Path, verb: &str, fragments: &[&str]) -> u32 {
    invocation(journal, verb, fragments).await.0
}

/// The value podman or buildah was given for `flag` in `argv`.
fn flag_value(argv: &str, flag: &str) -> String {
    let mut args = argv.split_whitespace();
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next().unwrap_or_default().to_string();
        }
    }
    panic!("{flag} was not in {argv:?}");
}

fn still_running(pid: u32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

/// Poll until `pid` leaves the process table. See `src/process_tests.rs` for
/// why `kill(pid, 0)` is both the liveness and the no-zombie assertion, and
/// why this has to be async.
async fn expect_gone(pid: u32) -> Duration {
    let started = Instant::now();
    while still_running(pid) {
        assert!(
            started.elapsed() < CEILING,
            "pid {pid} still occupied a process slot after {CEILING:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    started.elapsed()
}

fn image() -> ImageTag {
    ImageTag::new("example.invalid/cancellation:latest")
}

/// `Container::start` cancelled mid-`podman run`: no `Container` has been
/// constructed, so nothing that exists today would clean up. The name guard
/// covers the window -- the client dies and the reserved name is removed.
#[tokio::test]
async fn a_canceled_start_kills_the_client_and_removes_the_name() {
    let journal = fake_runtime();
    let name = unique_name("start");

    let tag = image();
    let start = Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None);
    let mut start = Box::pin(start);

    // Cancel only once `podman run` is demonstrably in flight, so the drop
    // lands in the window under test rather than before it.
    let running = [name.as_str()];
    let client = tokio::select! {
        _ = &mut start => panic!("the fake `podman run` should not have returned"),
        pid = invocation_pid(journal, "run", &running) => pid,
    };
    drop(start);

    let took = expect_gone(client).await;
    println!("canceled start: client {client} gone in {took:?}");
    expect_removed(journal, &name).await;
}

/// The same window reached the way a caller reaches it: `tokio::time::timeout`
/// drops the future it wrapped. This is the audit's original repro.
#[tokio::test]
async fn a_timed_out_start_leaves_no_client_and_no_name() {
    let journal = fake_runtime();
    let name = unique_name("timeout");

    let tag = image();
    let outcome = tokio::time::timeout(
        Duration::from_millis(400),
        Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None),
    )
    .await;
    assert!(
        outcome.is_err(),
        "the fake `podman run` sleeps, so this must time out"
    );

    let client = invocation_pid(journal, "run", &[name.as_str()]).await;
    let took = expect_gone(client).await;
    println!("timed-out start: client {client} gone in {took:?}");
    expect_removed(journal, &name).await;
}

/// Cancelled *during* `podman create`, before `podman init` has run.
#[tokio::test]
async fn a_canceled_create_removes_the_container() {
    let journal = fake_runtime();
    let name = unique_name("create");

    let options = ContainerCreateOptions::new(image(), ContainerLaunchSpec::default(), &name);
    let mut create = Box::pin(Container::create_initialized(options));

    let creating = [name.as_str()];
    let client = tokio::select! {
        _ = &mut create => panic!("the fake `podman create` should not have returned"),
        pid = invocation_pid(journal, "create", &creating) => pid,
    };
    drop(create);

    expect_gone(client).await;
    expect_removed(journal, &name).await;
}

/// Cancelled at the `create` -> `init` boundary: `create` has returned, so the
/// engine may hold a container, and the future is parked inside `init`. This
/// is the phase boundary with the most to lose, and the one where the pre-guard
/// code removed nothing at all on cancellation.
#[tokio::test]
async fn a_cancel_between_create_and_init_removes_the_container() {
    let journal = fake_runtime();
    let name = unique_name("init");
    // `create` returns at once; `init` is what sleeps. Scoping the marker to
    // `create` is what makes this the boundary case rather than a second copy
    // of the one above.
    return_immediately_for(journal, "create", &name);

    let options = ContainerCreateOptions::new(image(), ContainerLaunchSpec::default(), &name);
    let mut create = Box::pin(Container::create_initialized(options));

    // Waited on by verb: this name embeds `init`, so `podman create --name
    // outrig-cancel-init-...` contains the word too, and a fragment match
    // would park this test in `create` and quietly retire the boundary it
    // exists for. The name keeps the collision rather than dodging it, so the
    // matcher stays honest.
    let initializing = [name.as_str()];
    let client = tokio::select! {
        _ = &mut create => panic!("the fake `podman init` should not have returned"),
        pid = invocation_pid(journal, "init", &initializing) => pid,
    };
    drop(create);

    expect_gone(client).await;
    expect_removed(journal, &name).await;
}

/// A container that started successfully and was then dropped without `stop`
/// is still removed -- the pre-existing `Drop for Container` path, kept honest
/// now that it routes through the supervisor rather than an unreaped detached
/// spawn.
#[tokio::test]
async fn a_dropped_container_handle_removes_its_container() {
    let journal = fake_runtime();
    let name = unique_name("dropped");
    return_immediately_for(journal, "run", &name);

    let tag = image();
    let container =
        Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None)
            .await
            .expect("the fake `podman run` returns success");
    drop(container);

    // Removed by the attempt label the start stamped, which the handle keeps
    // -- every removal outrig issues for a container it made is scoped to the
    // attempt that made it, the handle's included. The fake records a removal
    // under what it actually removed, so the name is what this can observe.
    expect_removed(journal, &name).await;
}

/// A cancelled image build leaves no temporary tag.
///
/// The tag is generated inside the build, so the test cannot name it up
/// front -- which is the point: nothing outside the build knows it, so nothing
/// outside the build could have swept it. It is read back out of the recorded
/// `build` argv and the removal is required to name *that* tag. Matching only
/// the `outrig-tmp-` shape would let the concurrent label-commit test, which
/// also builds and removes one, satisfy this assertion while this build's own
/// cleanup was missing.
#[tokio::test]
async fn a_canceled_build_removes_its_temporary_tag() {
    let journal = fake_runtime();
    let context = tempfile::tempdir().expect("tempdir");
    std::fs::write(context.path().join("Dockerfile"), "FROM scratch\n").expect("write Dockerfile");

    let token = unique_name("built");
    let cfg = outrig::config::ImageConfig::from_dockerfile("Dockerfile", ".");
    let final_tag = ImageTag::new(format!("example.invalid/{token}:latest"));
    let build = outrig::image::build_image_for("built", &cfg, context.path(), &final_tag, false);
    let mut build = Box::pin(build);

    let building = [token.as_str()];
    let (client, argv) = tokio::select! {
        _ = &mut build => panic!("the fake `buildah build` should not have returned"),
        found = invocation(journal, "build", &building) => found,
    };
    let temp_tag = flag_value(&argv, "--tag");
    assert!(
        temp_tag.contains("outrig-tmp-"),
        "the build should target a temporary tag, got {temp_tag:?}"
    );
    drop(build);

    let took = expect_gone(client).await;
    println!("canceled build: buildah {client} gone in {took:?}, tag {temp_tag}");
    expect_removed(journal, &temp_tag).await;
}

/// A cancelled label-stamping pass leaves no buildah working container.
///
/// `buildah build` is steered to return at once so the future reaches
/// `buildah from`, which creates the working container and is where this
/// parks. The name is `outrig-label-<pid>-<nonce>`, generated inside the
/// commit and known to nothing outside it -- which is the reason it needs a
/// guard rather than a sweeper.
#[tokio::test]
async fn a_canceled_label_commit_removes_its_working_container() {
    let journal = fake_runtime();
    let context = tempfile::tempdir().expect("tempdir");
    std::fs::write(context.path().join("Dockerfile"), "FROM scratch\n").expect("write Dockerfile");

    let token = unique_name("labels");
    return_immediately_for(journal, "build", &token);

    let cfg = outrig::config::ImageConfig::from_dockerfile("Dockerfile", ".");
    let final_tag = ImageTag::new(format!("example.invalid/{token}:latest"));
    let build = outrig::image::build_image_for("labeled", &cfg, context.path(), &final_tag, false);
    let mut build = Box::pin(build);

    let from = ["outrig-label-", token.as_str()];
    let (client, argv) = tokio::select! {
        _ = &mut build => panic!("the fake `buildah from` should not have returned"),
        found = invocation(journal, "from", &from) => found,
    };
    let builder = flag_value(&argv, "--name");
    drop(build);

    let took = expect_gone(client).await;
    println!("canceled label commit: buildah {client} gone in {took:?}, builder {builder}");
    expect_removed(journal, &builder).await;
}

/// The `init` -> `start` boundary. `podman start --attach --interactive` is the
/// entrypoint-stdio sidecar's transport, spawned from `McpClient` rather than
/// from `Container`, so it is the one phase that does not run through
/// `container/mod.rs` at all -- worth its own case for exactly that reason.
///
/// The container is `Container::attach`ed, which constructs a handle without
/// touching an engine and, being unowned, will not try to remove anything on
/// drop. That keeps this case about the client and nothing else.
#[tokio::test]
async fn a_canceled_podman_start_attach_kills_its_client() {
    let journal = fake_runtime();
    let name = unique_name("attach");
    let logs = tempfile::tempdir().expect("tempdir");

    let container = Container::attach(name.clone(), image(), None, None);
    let connect = outrig::McpClient::connect_via_podman_start(
        &container,
        "sidecar",
        outrig::container::embedded::McpDeclarationSource::LaunchSpec,
        logs.path(),
    );
    let mut connect = Box::pin(connect);

    let starting = ["--attach", name.as_str()];
    let client = tokio::select! {
        _ = &mut connect => panic!("the fake `podman start` should not have returned"),
        pid = invocation_pid(journal, "start", &starting) => pid,
    };
    drop(connect);

    let took = expect_gone(client).await;
    println!("canceled podman start: client {client} gone in {took:?}");
}

/// A `podman run` that fails because the name is already in use must remove
/// **nothing**. The container holding that name belongs to someone else --
/// another session, a stray that outlived its record, one the user made by
/// hand -- and destroying it is a far worse outcome than the leak the guard
/// exists to prevent.
///
/// The guard removes by the attempt label it stamped on the container it asked
/// for, and a create that lost a collision made nothing carrying that label, so
/// this falls out of the mechanism rather than out of a special case.
#[tokio::test]
async fn a_name_collision_removes_nothing() {
    let journal = fake_runtime();
    let name = unique_name("collision");
    fail_name_collision_for(journal, "run", &name);

    let tag = image();
    let err = Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None)
        .await
        .expect_err("the fake reports the name as already in use");
    assert!(
        format!("{err}").contains("already in use"),
        "the collision should surface as podman reported it, got: {err}"
    );

    let attempt = attempt_label_of(journal, "run", &name).await;
    expect_not_removed(journal, &attempt, &name).await;
}

/// The same rule under cancellation, which is the harder half: outrig never
/// learns why the command ended, so it cannot tell a collision from a
/// container it made. Selecting by the attempt's own label is what lets it not
/// have to -- the label goes on in the creation request, so a container either
/// carries this attempt's and is its to remove, or does not exist. That is also
/// why it is not a `--cidfile`, which podman writes only *after* creating,
/// leaving an interval whose container the cleanup could not name.
#[tokio::test]
async fn a_canceled_start_over_a_taken_name_removes_nothing() {
    let journal = fake_runtime();
    let name = unique_name("collision-cancel");
    hold_before_creating_for(journal, "run", &name);

    let tag = image();
    let start = Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None);
    let mut start = Box::pin(start);

    drive_until_holding(journal, &mut start, &name).await;
    let attempt = attempt_label_of(journal, "run", &name).await;
    drop(start);

    expect_not_removed(journal, &attempt, &name).await;
}

/// The other reason a client can be alive having created nothing: podman has
/// simply not got there yet. Cancelled in that window, outrig must remove
/// nothing -- there is no container of its own to remove, and the name, if it
/// belongs to anything, belongs to somebody else.
///
/// Distinct from the collision case above in what it asserts, not only in what
/// it is called. A guard that had quietly stopped issuing removals altogether
/// would satisfy "nothing was removed" while having abandoned the obligation,
/// so this also requires the removal to be *issued* and scoped to this
/// attempt's label -- reaching nothing because nothing carries it, which is
/// the mechanism the whole design rests on rather than a special case for an
/// empty engine.
#[tokio::test]
async fn a_start_canceled_before_creation_issues_a_removal_that_reaches_nothing() {
    let journal = fake_runtime();
    let name = unique_name("precreate");
    hold_before_creating_for(journal, "run", &name);

    let tag = image();
    let start = Container::start_named(&tag, ContainerLaunchSpec::default(), name.clone(), None);
    let mut start = Box::pin(start);

    drive_until_holding(journal, &mut start, &name).await;

    let attempt = attempt_label_of(journal, "run", &name).await;
    drop(start);

    expect_removal_scoped_to(journal, &attempt).await;
    expect_not_removed(journal, &attempt, &name).await;
}

/// A caller cannot set outrig's own attempt label.
///
/// podman takes the last `--label` for a key, so a caller supplying this one
/// would replace the value cleanup filters on and silently disable it. It is
/// rejected before anything is spawned, and emitted after the caller's labels
/// besides, so neither half alone has to hold.
#[tokio::test]
async fn a_caller_cannot_claim_the_attempt_label() {
    let journal = fake_runtime();
    let name = unique_name("reserved");

    let mut launch = ContainerLaunchSpec::default();
    launch
        .labels
        .insert("org.outrig.attempt".to_string(), "hijacked".to_string());

    let tag = image();
    let err = Container::start_named(&tag, launch, name.clone(), None)
        .await
        .expect_err("the reserved label must be refused");
    assert!(
        format!("{err}").contains("reserved"),
        "the refusal should name the reason, got: {err}"
    );

    // Refused before the spawn, so podman was never asked to make anything.
    let started = invocations(journal)
        .into_iter()
        .filter(|(_, argv)| argv.contains(&name))
        .count();
    assert_eq!(started, 0, "nothing should have been spawned");
}
