//! Container lifecycle: start, stop, cleanup. Plus the [`add`] /
//! [`render`] submodules that scaffold new container-configs.
//!
//! Wraps `podman run`/`podman stop`/`podman rm` so callers get a typed
//! [`Container`] handle instead of poking podman directly. Cleanup is
//! defended in three layers, in order of preference:
//!
//! 1. [`Container::stop`] -- explicit teardown on the happy path.
//! 2. [`Drop`] -- best-effort detached `podman rm -f` if a `Container`
//!    falls out of scope without `stop` being called (e.g. a future was
//!    cancelled, an `?` propagated past the handle).
//! 3. [`install_panic_hook`] -- last-resort sweep over `TRACKED` when
//!    the process is unwinding from a panic and `Drop` cannot run.

#[cfg(feature = "internal")]
pub mod add;
pub mod embedded;
#[cfg(feature = "internal")]
pub mod render;

#[cfg(feature = "internal")]
pub use add::DOC_SYNC_FIELDS;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use nix::unistd::{Gid, Group, Uid, User};
use serde_json::Value;
use tokio::process::Child;

use crate::config::{CapabilityProfile, MountAccess, capability_name_without_prefix};
use crate::error::{OutrigError, Result};
use crate::image::ImageTag;
use crate::process::{self, Cmd, Transcript};
use crate::session::SessionId;

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
    transcript: Option<Transcript>,
    ownership: ContainerOwnership,
    disposed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerOwnership {
    Owned,
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInspect {
    pub image_tag: ImageTag,
    pub running: bool,
}

/// Complete inputs for a `podman run`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerLaunchSpec {
    pub workspace: Option<ContainerWorkspace>,
    pub mounts: Vec<ContainerMount>,
    pub capabilities: ContainerCapabilities,
}

impl ContainerLaunchSpec {
    pub fn workspace(host: impl Into<PathBuf>, container: impl Into<PathBuf>) -> Self {
        Self {
            workspace: Some(ContainerWorkspace {
                host: host.into(),
                container: container.into(),
            }),
            mounts: Vec::new(),
            capabilities: ContainerCapabilities::default(),
        }
    }
}

/// Primary read-write workspace mount. When present, this also sets `-w`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerWorkspace {
    pub host: PathBuf,
    pub container: PathBuf,
}

/// Extra bind mount. These do not affect the container working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMount {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

/// Linux capability policy applied to the container at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerCapabilities {
    pub profile: CapabilityProfile,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
}

impl Container {
    /// Start a container running `image`, applying the primary workspace and
    /// extra bind mounts from `launch`. A primary workspace is mounted
    /// read-write and used as the working directory; extra mounts never
    /// affect `-w`.
    pub async fn start(image: &ImageTag, launch: ContainerLaunchSpec) -> Result<Self> {
        let name = format!("outrig-{}", SessionId::new());
        Self::start_named(image, launch, name, None).await
    }

    /// Start with a caller-supplied container name. Session setup uses this
    /// so `session.json`, `container.log`, and the podman name all share one
    /// preallocated session id.
    pub(crate) async fn start_named(
        image: &ImageTag,
        launch: ContainerLaunchSpec,
        name: String,
        transcript: Option<Transcript>,
    ) -> Result<Self> {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        // Register before spawning so a SIGKILL between the spawn call and
        // its return can still be cleaned up by the panic hook.
        track(&name);

        let cmd = build_podman_run_cmd(image, &name, &launch, selinux_enforcing().await);

        if let Err(e) = process::run_capture_logged(cmd, "podman", transcript.as_ref()).await {
            untrack(&name);
            return Err(e);
        }

        let (host_workspace, container_workspace) = match &launch.workspace {
            Some(workspace) => (workspace.host.clone(), workspace.container.clone()),
            None => (PathBuf::new(), PathBuf::new()),
        };

        Ok(Self {
            name,
            image_tag: image.clone(),
            host_workspace,
            container_workspace,
            uid,
            gid,
            user_name: None,
            group_name: None,
            transcript,
            ownership: ContainerOwnership::Owned,
            disposed: false,
        })
    }

    /// Build a handle for an already-running container that outrig does not
    /// own. The caller is responsible for probing that the container exists
    /// and is running before constructing the handle.
    pub fn attach(
        name: impl Into<String>,
        image_tag: ImageTag,
        workspace: Option<(&Path, &Path)>,
        transcript: Option<Transcript>,
    ) -> Self {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let (host_workspace, container_workspace) = match workspace {
            Some((host, container)) => (host.to_path_buf(), container.to_path_buf()),
            None => (PathBuf::new(), PathBuf::new()),
        };

        Self {
            name: name.into(),
            image_tag,
            host_workspace,
            container_workspace,
            uid,
            gid,
            user_name: None,
            group_name: None,
            transcript,
            ownership: ContainerOwnership::Attached,
            disposed: false,
        }
    }

    /// Inspect an existing podman container by name. This is intentionally
    /// separate from [`Self::attach`] so callers can create session/log state
    /// before deciding whether to borrow the container.
    pub async fn inspect_existing(
        name: &str,
        transcript: Option<&Transcript>,
    ) -> Result<ContainerInspect> {
        let cmd = Cmd::new("podman").arg("inspect").arg(name);
        let output = process::run_capture_logged(cmd, "podman", transcript).await?;
        parse_container_inspect(name, &output.stdout)
    }

    /// Lightweight running-state probe used by attach-mode monitoring.
    /// A missing container is reported as `Ok(false)`; I/O failures still
    /// propagate because the caller cannot distinguish them from a broken
    /// podman environment.
    pub async fn is_running(name: &str) -> Result<bool> {
        let output = process::try_capture(
            Cmd::new("podman")
                .args(["inspect", "--format", "{{.State.Running}}"])
                .arg(name),
        )
        .await?;
        if !output.status.success() {
            return Ok(false);
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim() == "true")
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
        process::run_capture_logged(
            podman_exec_root(&self.name)
                .arg("mkdir")
                .arg("-p")
                .arg(&home),
            "podman",
            self.transcript.as_ref(),
        )
        .await?;
        process::run_capture_logged(
            podman_exec_root(&self.name)
                .arg("chown")
                .arg(format!("{user_name}:{group_name}"))
                .arg(&home),
            "podman",
            self.transcript.as_ref(),
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
        let probe = process::try_capture_logged(
            podman_exec_root(&self.name)
                .arg("getent")
                .arg(db)
                .arg(id.to_string()),
            "podman",
            self.transcript.as_ref(),
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
            let attempt =
                process::try_capture_logged(build(&name), "podman", self.transcript.as_ref())
                    .await?;
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
        if self.ownership == ContainerOwnership::Attached {
            self.disposed = true;
            return Ok(());
        }

        let secs = grace.as_secs().to_string();
        process::run_capture_logged(
            Cmd::new("podman")
                .args(["stop", "-t"])
                .arg(&secs)
                .arg(&self.name),
            "podman",
            self.transcript.as_ref(),
        )
        .await?;
        // `--rm` in start() makes this redundant on the success path, but
        // run it defensively in case `--rm` got disabled or the daemon
        // failed to honor it. try_capture so "no such container" doesn't
        // turn into an error.
        let _ = process::try_capture_logged(
            Cmd::new("podman").args(["rm", "-f"]).arg(&self.name),
            "podman",
            self.transcript.as_ref(),
        )
        .await;
        untrack(&self.name);
        self.disposed = true;
        Ok(())
    }

    pub(crate) fn transcript(&self) -> Option<Transcript> {
        self.transcript.clone()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.disposed || self.ownership == ContainerOwnership::Attached {
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
pub(super) fn podman_exec_root(name: &str) -> Cmd {
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

fn build_podman_run_cmd(
    image: &ImageTag,
    name: &str,
    launch: &ContainerLaunchSpec,
    selinux: bool,
) -> Cmd {
    let mut cmd = Cmd::new("podman")
        .args(["run", "-d", "--rm", "--name"])
        .arg(name);

    if let Some(workspace) = &launch.workspace {
        cmd = append_bind_mount(
            cmd,
            &workspace.host,
            &workspace.container,
            MountAccess::ReadWrite,
            selinux,
        );
    }
    for mount in &launch.mounts {
        cmd = append_bind_mount(cmd, &mount.host, &mount.container, mount.access, selinux);
    }

    cmd = cmd.arg("--userns=keep-id");
    if let Some(workspace) = &launch.workspace {
        cmd = cmd.arg("-w").arg(&workspace.container);
    }

    cmd = append_capability_flags(cmd, &launch.capabilities);

    cmd.args(["--security-opt=no-new-privileges", "--pull=never"])
        .arg(image.0.as_str())
        .args(["sleep", "infinity"])
}

fn append_capability_flags(mut cmd: Cmd, capabilities: &ContainerCapabilities) -> Cmd {
    match capabilities.profile {
        CapabilityProfile::Default => {}
        CapabilityProfile::NoNetRaw => {
            cmd = cmd.arg("--cap-drop=NET_RAW");
        }
        CapabilityProfile::DropAll => {
            cmd = cmd.arg("--cap-drop=ALL");
        }
    }

    for capability in &capabilities.cap_drop {
        cmd = cmd.arg(format!(
            "--cap-drop={}",
            capability_name_without_prefix(capability)
        ));
    }
    for capability in &capabilities.cap_add {
        cmd = cmd.arg(format!(
            "--cap-add={}",
            capability_name_without_prefix(capability)
        ));
    }

    cmd
}

fn append_bind_mount(
    cmd: Cmd,
    host: &Path,
    container: &Path,
    access: MountAccess,
    selinux: bool,
) -> Cmd {
    let mut opts = match access {
        MountAccess::ReadOnly => "ro".to_string(),
        MountAccess::ReadWrite => "rw".to_string(),
    };
    if selinux {
        opts.push_str(",Z");
    }
    cmd.arg("-v")
        .arg(format!("{}:{}:{opts}", host.display(), container.display()))
}

/// Install a process-wide panic hook that sweeps `TRACKED` with
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

fn parse_container_inspect(name: &str, stdout: &[u8]) -> Result<ContainerInspect> {
    let value: Value = serde_json::from_slice(stdout).map_err(|source| {
        OutrigError::Configuration(format!("podman inspect {name:?}: invalid JSON: {source}"))
    })?;
    let object = value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_object)
        .ok_or_else(|| {
            OutrigError::Configuration(format!(
                "podman inspect {name:?}: expected a non-empty JSON array"
            ))
        })?;

    let running = object
        .get("State")
        .and_then(|state| state.get("Running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let image = object
        .get("ImageName")
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("Config")
                .and_then(|config| config.get("Image"))
                .and_then(Value::as_str)
        })
        .or_else(|| object.get("Image").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            OutrigError::Configuration(format!("podman inspect {name:?}: missing image name"))
        })?;

    Ok(ContainerInspect {
        image_tag: ImageTag(image.to_string()),
        running,
    })
}

#[cfg(any(test, feature = "e2e"))]
pub fn is_tracked(name: &str) -> bool {
    TRACKED.lock().map(|g| g.contains(name)).unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(cmd: Cmd) -> Vec<String> {
        std::iter::once(cmd.program.to_string())
            .chain(
                cmd.args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect()
    }

    #[test]
    fn podman_run_args_include_workspace_then_extra_mounts() {
        let launch = ContainerLaunchSpec {
            workspace: Some(ContainerWorkspace {
                host: "/host/repo".into(),
                container: "/workspace".into(),
            }),
            mounts: vec![
                ContainerMount {
                    host: "/host/docs".into(),
                    container: "/resources/docs".into(),
                    access: MountAccess::ReadOnly,
                },
                ContainerMount {
                    host: "/host/cache".into(),
                    container: "/resources/cache".into(),
                    access: MountAccess::ReadWrite,
                },
            ],
            capabilities: ContainerCapabilities::default(),
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag("local:test".to_string()),
            "outrig-test",
            &launch,
            false,
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "run",
                "-d",
                "--rm",
                "--name",
                "outrig-test",
                "-v",
                "/host/repo:/workspace:rw",
                "-v",
                "/host/docs:/resources/docs:ro",
                "-v",
                "/host/cache:/resources/cache:rw",
                "--userns=keep-id",
                "-w",
                "/workspace",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_apply_selinux_to_every_mount_without_workdir() {
        let launch = ContainerLaunchSpec {
            workspace: None,
            mounts: vec![ContainerMount {
                host: "/host/docs".into(),
                container: "/resources/docs".into(),
                access: MountAccess::ReadOnly,
            }],
            capabilities: ContainerCapabilities::default(),
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag("local:test".to_string()),
            "outrig-test",
            &launch,
            true,
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "run",
                "-d",
                "--rm",
                "--name",
                "outrig-test",
                "-v",
                "/host/docs:/resources/docs:ro,Z",
                "--userns=keep-id",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_include_no_net_raw_profile() {
        let launch = ContainerLaunchSpec {
            workspace: None,
            mounts: Vec::new(),
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::NoNetRaw,
                cap_drop: Vec::new(),
                cap_add: Vec::new(),
            },
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag("local:test".to_string()),
            "outrig-test",
            &launch,
            false,
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "run",
                "-d",
                "--rm",
                "--name",
                "outrig-test",
                "--userns=keep-id",
                "--cap-drop=NET_RAW",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_render_drop_all_before_explicit_adds() {
        let launch = ContainerLaunchSpec {
            workspace: None,
            mounts: Vec::new(),
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::DropAll,
                cap_drop: vec!["CAP_MKNOD".to_string()],
                cap_add: vec!["CAP_NET_BIND_SERVICE".to_string()],
            },
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag("local:test".to_string()),
            "outrig-test",
            &launch,
            false,
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "run",
                "-d",
                "--rm",
                "--name",
                "outrig-test",
                "--userns=keep-id",
                "--cap-drop=ALL",
                "--cap-drop=MKNOD",
                "--cap-add=NET_BIND_SERVICE",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }
}
