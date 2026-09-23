//! A session is a Python session: `Outrig::launch` unpacks the interpreter
//! this build embedded before it starts any container, and refuses a mount
//! that would land where the interpreter goes. No setup step stands between
//! `cargo build` and either.
//!
//! Needs no podman. The binary's `XDG_CACHE_HOME` points at an empty cache,
//! and `podman`/`buildah` on its `PATH` are fakes that fail after leaving a
//! mark, so a launch gets exactly as far as its first container call.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use outrig::{LaunchSpec, MountAccess, MountSpec, Outrig, WorkspaceSpec};

/// Leaves a mark for every call, then fails the way a missing image would.
const FAKE: &str = "#!/bin/sh\ntouch \"$OUTRIG_PAYLOAD_TEST_MARK\"\nexit 1\n";

struct Env {
    cache: PathBuf,
    mark: PathBuf,
}

/// The binary's one environment, written once. `XDG_CACHE_HOME` and `PATH`
/// are read by every process, so a per-test variable name cannot apply;
/// `OnceLock::get_or_init` blocks every other caller until the write is done,
/// and every test here calls this before anything else.
fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("outrig-python-payload")
            .tempdir()
            .expect("tempdir");
        let cache = root.path().join("cache");
        let bin = root.path().join("bin");
        let mark = root.path().join("runtime-was-called");
        std::fs::create_dir_all(&cache).expect("create cache");
        std::fs::create_dir_all(&bin).expect("create bin");
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
        // multi-thread races. These writes happen inside `get_or_init`, which
        // every test in this binary enters before doing anything else, so no
        // thread reads any of them concurrently with the write.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", &cache);
            std::env::set_var("PATH", path);
            std::env::set_var("OUTRIG_PAYLOAD_TEST_MARK", &mark);
        }

        // Outlives every test in the binary; nothing runs after the last. What
        // it holds is a few bytes, once the test that unpacks has cleaned up.
        std::mem::forget(root);
        Env { cache, mark }
    })
}

/// Removes a directory when dropped, which unwinding does too -- a static
/// tempdir's destructor would not run at process exit.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn launch_error(spec: &LaunchSpec) -> String {
    match Outrig::launch(spec).await {
        Ok(_) => panic!("launch succeeded against a fake podman"),
        Err(err) => err.to_string(),
    }
}

/// The payload is in the cache and runs, and the launch went on to the
/// container runtime -- which it only reaches after the payload is in place.
#[tokio::test]
async fn the_first_launch_unpacks_a_working_interpreter_before_any_container() {
    let env = env();
    // About 170 MB, which would otherwise outlive the run in the temp dir.
    let _unpacked = RemoveOnDrop(env.cache.join("outrig"));
    let logs = tempfile::tempdir().expect("tempdir");
    let spec = LaunchSpec::from_image(
        "localhost/never-pulled",
        BTreeMap::new(),
        logs.path().into(),
    );
    let err = launch_error(&spec).await;
    assert!(env.mark.exists(), "the launch stopped before podman: {err}");

    // Beside it is the lock the unpack took, which is a file.
    let unpacked: Vec<PathBuf> = std::fs::read_dir(env.cache.join("outrig/python"))
        .expect("the launch unpacked into the cache")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.is_dir())
        .collect();
    let [payload] = unpacked.as_slice() else {
        panic!("expected one unpacked payload and no stage left behind: {unpacked:?}");
    };
    let printed = Command::new(payload.join("bin/python3"))
        .args(["-I", "-c", "print(1)"])
        .output()
        .expect("run the unpacked interpreter");
    assert!(printed.status.success(), "{printed:?}");
    assert_eq!(String::from_utf8_lossy(&printed.stdout), "1\n");
}

#[tokio::test]
async fn a_workspace_or_mount_under_outrig_is_refused() {
    env();
    let logs = tempfile::tempdir().expect("tempdir");
    let host = tempfile::tempdir().expect("tempdir");
    let base = || {
        LaunchSpec::from_image(
            "localhost/never-pulled",
            BTreeMap::new(),
            logs.path().into(),
        )
    };

    let cases: [(&str, LaunchSpec); 2] = [
        (
            "/outrig/work",
            base().with_workspace(WorkspaceSpec::new(host.path(), "/outrig/work")),
        ),
        (
            "/outrig",
            base().with_mount(MountSpec::new(
                host.path(),
                "/outrig",
                MountAccess::ReadOnly,
            )),
        ),
    ];
    for (destination, spec) in cases {
        let err = launch_error(&spec).await;
        assert!(
            err.contains(&format!("container path {destination} is under /outrig")),
            "{destination}: {err}"
        );
    }
}
