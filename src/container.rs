//! Container lifecycle: start, stop, cleanup.
//!
//! Wraps `podman run`/`podman stop`/`podman rm` so callers get a typed
//! [`Container`] handle instead of poking podman directly. Cleanup is
//! defended in three layers, in order of preference:
//!
//! 1. [`Container::stop`] -- explicit teardown on the happy path.
//! 2. [`Drop`] -- best-effort detached `podman rm -f` if a `Container`
//!    falls out of scope without `stop` being called (e.g. a future was
//!    cancelled, an `?` propagated past the handle).
//! 3. [`install_panic_hook`] -- last-resort sweep over [`TRACKED`] when
//!    the process is unwinding from a panic and `Drop` cannot run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use jiff::Zoned;
use rand::RngCore;

use crate::error::Result;
use crate::image::ImageTag;
use crate::process::{self, Cmd};

static TRACKED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Debug)]
pub struct Container {
    pub name: String,
    pub image_tag: ImageTag,
    pub host_workspace: PathBuf,
    pub container_workspace: PathBuf,
    pub uid: u32,
    pub gid: u32,
    disposed: bool,
}

impl Container {
    pub async fn start(image: &ImageTag, host_ws: &Path, ws_container: &Path) -> Result<Self> {
        let name = format!("outrig-{}", session_id());
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        let mount_opts = if selinux_enforcing().await {
            "rw,Z"
        } else {
            "rw"
        };
        let mount = format!(
            "{}:{}:{mount_opts}",
            host_ws.display(),
            ws_container.display()
        );

        // Register before spawning so a SIGKILL between the spawn call and
        // its return can still be cleaned up by the panic hook.
        track(&name);

        let cmd = Cmd::new("podman")
            .args(["run", "-d", "--rm", "--name"])
            .arg(&name)
            .arg("-v")
            .arg(&mount)
            .args(["--userns=keep-id", "-w"])
            .arg(ws_container)
            .args(["--security-opt=no-new-privileges", "--pull=never"])
            .arg(image.0.as_str())
            .args(["sleep", "infinity"]);

        if let Err(e) = process::run_capture(cmd).await {
            untrack(&name);
            return Err(e);
        }

        Ok(Self {
            name,
            image_tag: image.clone(),
            host_workspace: host_ws.to_path_buf(),
            container_workspace: ws_container.to_path_buf(),
            uid,
            gid,
            disposed: false,
        })
    }

    pub async fn stop(mut self, grace: Duration) -> Result<()> {
        let secs = grace.as_secs().to_string();
        process::run_capture(
            Cmd::new("podman")
                .args(["stop", "-t"])
                .arg(&secs)
                .arg(&self.name),
        )
        .await?;
        // `--rm` in start() makes this redundant on the success path, but
        // run it defensively in case `--rm` got disabled or the daemon
        // failed to honor it. try_capture so "no such container" doesn't
        // turn into an error.
        let _ = process::try_capture(Cmd::new("podman").args(["rm", "-f"]).arg(&self.name)).await;
        untrack(&self.name);
        self.disposed = true;
        Ok(())
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.disposed {
            return;
        }
        spawn_detached_rm(&self.name);
        untrack(&self.name);
    }
}

/// Best-effort `podman rm -f <name>` with stdio nulled. Synchronous,
/// detached, requires no tokio runtime -- safe from `Drop` and panic hooks.
fn spawn_detached_rm(name: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Install a process-wide panic hook that sweeps [`TRACKED`] with
/// `podman rm -f` before delegating to the previous hook. Idempotent --
/// safe to call from multiple `main`s or test setups.
pub fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(g) = TRACKED.lock() {
                for name in g.iter() {
                    spawn_detached_rm(name);
                }
            }
            prev(info);
        }));
    });
}

fn track(name: &str) {
    if let Ok(mut g) = TRACKED.lock() {
        g.insert(name.to_string());
    }
}

fn untrack(name: &str) {
    if let Ok(mut g) = TRACKED.lock() {
        g.remove(name);
    }
}

#[cfg(any(test, feature = "e2e"))]
pub fn is_tracked(name: &str) -> bool {
    TRACKED.lock().map(|g| g.contains(name)).unwrap_or(false)
}

fn session_id() -> String {
    let ts = Zoned::now()
        .with_time_zone(jiff::tz::TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%SZ");
    let mut buf = [0u8; 2];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("{ts}-{:02x}{:02x}", buf[0], buf[1])
}

async fn selinux_enforcing() -> bool {
    if let Ok(out) = process::try_capture(Cmd::new("getenforce")).await
        && out.status.success()
    {
        return String::from_utf8_lossy(&out.stdout).trim() == "Enforcing";
    }
    match tokio::fs::read_to_string("/sys/fs/selinux/enforce").await {
        Ok(s) => s.trim() == "1",
        Err(_) => false,
    }
}
