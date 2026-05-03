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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use jiff::Zoned;
use nix::unistd::{Gid, Group, Uid, User};
use rand::RngCore;
use tokio::process::Child;

use crate::error::{OutrigError, Result};
use crate::image::ImageTag;
use crate::process::{self, Cmd};

/// Maximum `_`-suffix retries before bootstrap gives up.
const BOOTSTRAP_RETRIES: usize = 10;

static TRACKED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Debug)]
pub struct Container {
    pub name: String,
    pub image_tag: ImageTag,
    pub host_workspace: PathBuf,
    pub container_workspace: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// In-container user name resolved by [`Container::bootstrap_user`].
    /// `None` until bootstrap has run.
    pub user_name: Option<String>,
    /// In-container group name resolved by [`Container::bootstrap_user`].
    /// `None` until bootstrap has run.
    pub group_name: Option<String>,
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
            user_name: None,
            group_name: None,
            disposed: false,
        })
    }

    /// Materialize an in-container user+group matching the host UID/GID,
    /// reusing existing entries when present and appending `_` to candidate
    /// names on collision.
    ///
    /// All `podman exec` calls here go through `--user=0:0` to land at
    /// in-container UID 0; under `--userns=keep-id` an unscoped exec would
    /// default to the host user, which can't `useradd` / `groupadd` / write
    /// to `/home`. Records the resolved names on the struct for
    /// [`Container::exec_stdio`] to reference. Must be called once, after
    /// [`Container::start`], before any host-user-scoped exec.
    pub async fn bootstrap_user(&mut self) -> Result<()> {
        let host_user = User::from_uid(Uid::from_raw(self.uid))
            .ok()
            .flatten()
            .map(|u| u.name)
            .unwrap_or_else(|| format!("u{}", self.uid));
        let host_group = Group::from_gid(Gid::from_raw(self.gid))
            .ok()
            .flatten()
            .map(|g| g.name)
            .unwrap_or_else(|| format!("g{}", self.gid));

        let group_name = self.resolve_or_create_group(&host_group).await?;
        let user_name = self.resolve_or_create_user(&host_user, &group_name).await?;

        let home = format!("/home/{user_name}");
        process::run_capture(
            podman_exec_root(&self.name)
                .arg("mkdir")
                .arg("-p")
                .arg(&home),
        )
        .await?;
        process::run_capture(
            podman_exec_root(&self.name)
                .arg("chown")
                .arg(format!("{user_name}:{group_name}"))
                .arg(&home),
        )
        .await?;

        self.user_name = Some(user_name);
        self.group_name = Some(group_name);
        Ok(())
    }

    async fn resolve_or_create_group(&self, candidate: &str) -> Result<String> {
        if let Some(name) = self.probe_entry("group", self.gid).await? {
            return Ok(name);
        }
        self.create_with_retry(candidate, "group", |name| {
            podman_exec_root(&self.name)
                .arg("groupadd")
                .arg("--gid")
                .arg(self.gid.to_string())
                .arg(name)
        })
        .await
    }

    async fn resolve_or_create_user(&self, candidate: &str, group: &str) -> Result<String> {
        if let Some(name) = self.probe_entry("passwd", self.uid).await? {
            return Ok(name);
        }
        self.create_with_retry(candidate, "user", |name| {
            podman_exec_root(&self.name)
                .arg("useradd")
                .arg("-u")
                .arg(self.uid.to_string())
                .arg("-g")
                .arg(group)
                .arg(name)
        })
        .await
    }

    /// Look up an existing entry in the in-container `getent` database
    /// (`group` or `passwd`) by id. Returns the first `:`-field of the first
    /// matching line, or `None` if `getent` exited non-zero (no match).
    async fn probe_entry(&self, db: &str, id: u32) -> Result<Option<String>> {
        let probe = process::try_capture(
            podman_exec_root(&self.name)
                .arg("getent")
                .arg(db)
                .arg(id.to_string()),
        )
        .await?;
        if !probe.status.success() {
            return Ok(None);
        }
        Ok(first_colon_field(&probe.stdout))
    }

    /// Try `build(candidate)`; on non-zero exit, append `_` and retry up to
    /// [`BOOTSTRAP_RETRIES`] times. `kind` labels the entity in the
    /// exhausted-error message (e.g. `"group"`, `"user"`).
    async fn create_with_retry<F>(
        &self,
        candidate: &str,
        kind: &'static str,
        mut build: F,
    ) -> Result<String>
    where
        F: FnMut(&str) -> Cmd,
    {
        let mut name = candidate.to_string();
        for _ in 0..BOOTSTRAP_RETRIES {
            let attempt = process::try_capture(build(&name)).await?;
            if attempt.status.success() {
                return Ok(name);
            }
            name.push('_');
        }
        Err(OutrigError::BootstrapExhausted { kind })
    }

    /// Build the argv for a `podman exec -i --user --env HOME ...` invocation
    /// without spawning. `HOME` is always set to the in-container home
    /// directory; entries in `env` are forwarded via `--env K=V` (BTreeMap
    /// order makes the resulting argv deterministic).
    ///
    /// Panics if [`Container::bootstrap_user`] has not yet been called --
    /// the user/group don't exist inside the container, so a `--user`-scoped
    /// exec would fail at the podman layer with a less useful message.
    pub fn build_exec_argv(&self, cmd: &[String], env: &BTreeMap<String, String>) -> Cmd {
        let user_name = self
            .user_name
            .as_deref()
            .expect("bootstrap_user must be called before build_exec_argv");

        let mut c = Cmd::new("podman")
            .args(["exec", "-i"])
            .arg(format!("--user={}:{}", self.uid, self.gid))
            .arg("--env")
            .arg(format!("HOME=/home/{user_name}"));
        for (k, v) in env {
            c = c.arg("--env").arg(format!("{k}={v}"));
        }
        c = c.arg(&self.name);
        for arg in cmd {
            c = c.arg(arg);
        }
        c
    }

    /// Suffix appended to `outrig-` in the container name -- the timestamp +
    /// short random hex generated at start time. Falls back to the full name
    /// if the prefix isn't present (defensive against name-format changes).
    pub fn session_suffix(&self) -> &str {
        self.name.strip_prefix("outrig-").unwrap_or(&self.name)
    }

    /// Spawn a command inside the container as the host user, with all three
    /// stdio streams piped back to the caller.
    pub async fn exec_stdio(
        &self,
        cmd: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Child> {
        process::spawn_stdio(self.build_exec_argv(cmd, env)).await
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

/// `podman exec --user=0:0 <name> ...`, i.e. running as the container's
/// root regardless of how the container was started. Used by
/// [`Container::bootstrap_user`] -- under `--userns=keep-id`, an unscoped
/// `podman exec` defaults to the *host* user, which can't `useradd` /
/// `groupadd` / write to `/home`. Forcing `--user=0:0` explicitly puts us
/// at in-container UID 0, which is what we need before any host user
/// exists inside the container.
fn podman_exec_root(name: &str) -> Cmd {
    Cmd::new("podman").args(["exec", "--user=0:0"]).arg(name)
}

/// Parse the first `:`-separated field of the first line of `getent`-style
/// output. `tgockel:x:1000:` -> `Some("tgockel")`. Returns `None` for empty
/// or malformed input. Stdout is lossy-decoded; this is fine for entries
/// in `/etc/passwd` and `/etc/group`, which are ASCII in practice.
fn first_colon_field(stdout: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(stdout);
    let line = line.lines().next()?;
    let name = line.split(':').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
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
