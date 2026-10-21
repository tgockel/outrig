//! #422 through the public API: an entrypoint-stdio container that exits at once, whose attach
//! client reports an exit code and none of what the container wrote.
//!
//! Deliberately **not** `e2e`-gated. `podman` is a shell script, so this runs in the ordinary
//! suite. What it proves is the wiring -- `create_initialized` names a log, the create argv
//! carries it, `connect_via_podman_start` reads it when the client says nothing -- which no real
//! engine can be made to prove on demand, because the loss is a race inside podman. That the log
//! has the line when attach loses it is what `tests/library_surface.rs`'s unresolvable-entrypoint
//! case checks against a real engine, on every run it is lost.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use outrig::McpClient;
use outrig::container::embedded::McpDeclarationSource;
use outrig::container::{Container, ContainerCreateOptions, ContainerLaunchSpec};
use outrig::error::OutrigError;
use outrig::image::ImageTag;

/// A `podman` whose containers log where `--log-opt=path=` says and whose `start --attach`
/// client never relays a byte: the shape of the loss.
///
/// State is per container name, so concurrent tests in this binary cannot read each other's. The
/// `start` is told the id the `create` printed rather than the name, so the log path is kept under
/// that too.
const FAKE_PODMAN: &str = r#"#!/bin/sh
dir=$(dirname "$0")
name=
log=
for arg in "$@"; do
  case "$arg" in
    --name) name=next ;;
    --log-opt=path=*) log=${arg#--log-opt=path=} ;;
    *) [ "$name" = next ] && name=$arg ;;
  esac
done

case "$1" in
  create)
    printf '%s\n' "$*" > "$dir/$name.create.argv"
    printf '%s' "$log" > "$dir/$name.log.path"
    # What `podman create` prints: the container's id, 64 hex digits.
    id=$(printf '%064x' "$$")
    printf '%s' "$log" > "$dir/$id.log.path"
    printf '%s\n' "$id"
    ;;
  start)
    # The container ran and wrote what conmon logs: a line of protocol on stdout, and the
    # launcher's diagnostic on stderr, in two pieces as an unbuffered `eprintln!` makes it.
    id=$(eval echo "\${$#}")
    log=$(cat "$dir/$id.log.path" 2>/dev/null)
    if [ -n "$log" ]; then
      ts=2026-10-13T14:30:00.123456789-06:00
      printf '%s stdout F %s\n' "$ts" '{"jsonrpc":"2.0","id":0}' >> "$log"
      printf '%s stderr P %s\n' "$ts" 'outrig-enter: open outrig-no-such-program' >> "$log"
      printf '%s stderr F %s\n' "$ts" ': No such file or directory (not found in PATH=/usr/bin)' >> "$log"
    fi
    # And the client reports the exit code and nothing else.
    exit 1
    ;;
esac
exit 0
"#;

/// Put the fake first on `PATH` and return the directory it keeps its state in.
///
/// `PATH` is process-global, so this happens once, before any test spawns anything -- the same
/// arrangement, for the same reason, as `tests/cancellation.rs`.
fn fake_podman() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let root = tempfile::Builder::new()
            .prefix("outrig-fake-podman")
            .tempdir()
            .expect("tempdir");
        let podman = root.path().join("podman");
        std::fs::write(&podman, FAKE_PODMAN).expect("write the fake");
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the fake");

        let mut path = std::ffi::OsString::from(root.path());
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread races. This is
        // inside `get_or_init`, which every test here enters before it spawns anything, so no
        // thread reads `PATH` concurrently with the write.
        unsafe { std::env::set_var("PATH", path) };

        // Outlives every test in the binary; nothing runs after the last one.
        let dir = root.path().to_path_buf();
        std::mem::forget(root);
        dir
    })
}

/// An SELinux host does not get a container log, and so has nothing to recover from.
fn selinux_enforcing() -> bool {
    std::fs::read_to_string("/sys/fs/selinux/enforce").is_ok_and(|enforce| enforce.trim() == "1")
}

#[tokio::test]
async fn a_failed_start_whose_attach_client_said_nothing_reports_what_the_container_logged() {
    if selinux_enforcing() {
        eprintln!("skipped: no container log is made where SELinux is enforcing");
        return;
    }
    let fake = fake_podman();
    let name = format!("outrig-attach-recovery-{}", std::process::id());
    let logs = tempfile::tempdir().expect("tempdir");

    let created = Container::create_initialized(ContainerCreateOptions::new(
        ImageTag::new("example.invalid/attach-recovery:latest"),
        ContainerLaunchSpec::default(),
        &name,
    ))
    .await
    .expect("the fake accepts a create");

    // The create named a log, and told podman how to keep it.
    let argv = std::fs::read_to_string(fake.join(format!("{name}.create.argv"))).expect("argv");
    assert!(argv.contains(" --log-driver=json-file "), "{argv}");
    assert!(argv.contains(" --log-opt=tag= "), "{argv}");
    assert!(argv.contains(" --log-opt=max-size=1mb "), "{argv}");
    let log_path = std::fs::read_to_string(fake.join(format!("{name}.log.path"))).expect("path");
    assert!(
        argv.contains(&format!(" --log-opt=path={log_path} ")),
        "{argv}"
    );
    assert!(Path::new(&log_path).is_absolute(), "{log_path}");

    // Started, the container's client says nothing, and the error says what the container did.
    let started = std::time::Instant::now();
    let err = McpClient::connect_via_podman_start(
        &created,
        "badentry",
        McpDeclarationSource::LaunchSpec,
        logs.path(),
    )
    .await
    .map(drop)
    .expect_err("a server that exits at once does not connect");

    let OutrigError::McpStartupFailed(failure) = &err else {
        panic!("expected McpStartupFailed, got {err:?}");
    };
    let expected = "outrig-enter: open outrig-no-such-program: No such file or directory \
                    (not found in PATH=/usr/bin)";
    assert_eq!(failure.exit, "code 1");
    assert_eq!(failure.stderr_tail, expected, "{err}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "the settle's five seconds were waited out: {:?}",
        started.elapsed()
    );
    // The file the error cites, and `outrig logs` reads, agrees with it.
    assert_eq!(
        std::fs::read_to_string(logs.path().join("badentry.stderr")).expect("stderr file"),
        format!("{expected}\n")
    );

    // The log goes with the container's handle.
    drop(created);
    assert!(!Path::new(&log_path).exists(), "{log_path} was left behind");
}
