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
//! 3. [`install_panic_hook`] -- last-resort sweep over `TRACKED` when
//!    the process is unwinding from a panic and `Drop` cannot run.

pub mod embedded;
pub mod enter;
mod namespace;
pub mod sidecar;
mod userdb;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Mutex, OnceLock};

use std::time::Duration;
use tokio::sync::OnceCell;

use nix::unistd::{Gid, Group, Uid, User};
use serde_json::Value;
use tokio::process::Child;

use crate::config::{CapabilityProfile, MountAccess, capability_name_without_prefix};
use crate::error::{OutrigError, Result};
use crate::image::ImageTag;
use crate::process::{self, Cmd, Transcript};
use crate::supervise::Reissue;

/// Maximum `_`-suffix retries before bootstrap gives up.
const BOOTSTRAP_RETRIES: usize = 10;

/// Marks the container that one `Container::start_named` or
/// `create_initialized` attempt asked podman to create.
///
/// Cleanup filters on this with `podman rm --filter`, which arrived in podman
/// 4.3 (it is absent from 4.2's `podman-rm` man page and present in 4.3's). Its value is fresh per
/// attempt; see [`NameGuard`] for why cleanup is scoped to it and not to the
/// container name.
const ATTEMPT_LABEL: &str = "org.outrig.attempt";

/// Length of the container id podman prints from a `create` or a detached
/// `run`: a full sha256, in lowercase hex. Anything else is not an id.
const PODMAN_ID_LEN: usize = 64;

/// Floor on the budget [`Container::stop`] gives its removal client.
///
/// `stop`'s `grace` is what podman waits for the *container's* processes, and
/// zero is a legitimate value there -- "do not wait, kill now". The removal
/// that follows is a different command with a different job, so a zero grace
/// must not silently reduce it to no attempt at all.
///
/// Calibrated to separate *wedged* from *slow*, and deliberately far to the
/// slow side. A healthy `podman rm -f` returns in well under a second, but a
/// loaded machine -- a parallel test suite, a busy engine -- can stretch that
/// by an order of magnitude without anything being wrong, and cutting the
/// client short there trades a hang that was not happening for a removal that
/// has to be retried. The bound exists for the client that will never return.
const MIN_REMOVAL_BUDGET: Duration = Duration::from_secs(30);

static TRACKED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Debug)]
pub struct Container {
    name: String,
    image_tag: ImageTag,
    host_workspace: PathBuf,
    container_workspace: PathBuf,
    uid: u32,
    gid: u32,
    /// In-container user name resolved by [`Container::bootstrap_user`].
    /// `None` until bootstrap has run.
    user_name: Option<String>,
    /// In-container group name resolved by [`Container::bootstrap_user`].
    /// `None` until bootstrap has run.
    group_name: Option<String>,
    transcript: Option<Transcript>,
    ownership: ContainerOwnership,
    /// Whether the interceptor's resolver was baked in at `podman create`
    /// (`--dns` flags). [`NetworkInterceptor::attach`] skips its exec-based
    /// resolv.conf install for such containers -- necessarily, since a
    /// created-but-not-started container cannot be exec'd.
    ///
    /// [`NetworkInterceptor::attach`]: crate::network::NetworkInterceptor::attach
    dns_preconfigured: bool,
    /// Init PID, cached by [`Container::pid`] after the first `podman
    /// inspect`.
    pid: OnceCell<u32>,
    /// The engine's own id for this container, from the `create`/`run` that
    /// made it. `None` for a container outrig only borrowed, which it never
    /// stops.
    ///
    /// A name is a request and can be granted again; an id is the engine's and
    /// is never handed out twice. Anything this handle does *to* the container
    /// goes through the id, so a handle kept for a retry -- which is the whole
    /// point of [`stop_or_keep`](Self::stop_or_keep) -- cannot act on whatever
    /// holds the name by the time that retry runs.
    id: Option<ContainerId>,
    /// Stands in for the engine call whose rendering contains the fragment,
    /// first match winning, so a test can wedge the stop or the removal in
    /// particular and hold the other one still. Every bounded-call path here
    /// ends at a `podman` a test cannot install.
    #[cfg(test)]
    engine_override: Vec<(&'static str, Cmd)>,
    /// How long each bounded engine call gets. `None` is the real budget,
    /// which is tens of seconds -- too long to wait out in a test, and not
    /// something a paused clock can help with, since a bounded call waiting on
    /// a real child leaves the runtime idle and every deadline in the test
    /// fires at once.
    #[cfg(test)]
    engine_budget: Option<Duration>,
    /// The attempt token stamped on this container at creation, for a removal
    /// that has to name it after the fact. `None` for an attached container,
    /// which is nobody's here to remove. See [`removal_cmd`].
    attempt: Option<String>,
    disposed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerOwnership {
    Owned,
    Attached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerInspect {
    pub image_tag: ImageTag,
    pub running: bool,
}

/// Podman container label carrying the owning session id. Set on every
/// session-owned container (primary and sidecars) so `outrig clean` can sweep
/// strays even when the session record is lost.
pub const LABEL_SESSION: &str = "org.outrig.session";

/// Podman container label carrying a sidecar's config name. Set only on
/// sidecar containers.
pub const LABEL_SIDECAR: &str = "org.outrig.sidecar";

/// The canonical sidecar container name, `outrig-<sid>-<sc>`. Load-bearing:
/// the watcher reaps by it, session records store it, and `/sidecar list`
/// looks containers up by it -- every producer must use this one scheme.
pub fn sidecar_container_name(session_suffix: &str, sidecar: &str) -> String {
    format!("outrig-{session_suffix}-{sidecar}")
}

/// Where `outrig-enter` grafts the sidecar's own rootfs -- its `--graft`, and
/// the prefix OutRig applies to image-supplied program paths.
pub const PRIMARY_VIEW_GRAFT: &str = "/mnt";
/// Bind target for the primary's `/proc/<pid>/ns` directory; the launcher's
/// `--ns-file` is this plus [`PRIMARY_VIEW_NS_FILE`].
pub const PRIMARY_VIEW_NS_MOUNT: &str = "/target-ns";
/// The mount-namespace entry inside a `/proc/<pid>/ns` directory. The launcher
/// joins `<PRIMARY_VIEW_NS_MOUNT>/<this>`. It renders `mnt`, which is *not*
/// [`PRIMARY_VIEW_GRAFT`] (`/mnt`) despite the coincidence -- this is the nsfs
/// file named `mnt`, that is the graft directory.
pub const PRIMARY_VIEW_NS_FILE: &str = "mnt";
/// Bind target for the materialized launcher, and its `--entrypoint`.
pub const PRIMARY_VIEW_HELPER_MOUNT: &str = "/outrig-enter";

/// Inputs for a `view = "primary"` sidecar: which primary it joins and the
/// launcher to bind in. Present only on that placement mode. It swaps
/// `--userns=keep-id` for `--userns=container:<primary>`, adds
/// `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE`, binds the primary's nsfs directory and the
/// launcher (both plain `:ro`, never SELinux-relabeled), sets
/// `--entrypoint /outrig-enter`, and gives the payload a `HOME` it can write.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrimaryView {
    /// Primary container name, for `--userns=container:<name>`.
    pub primary_container: String,
    /// Primary init PID, for `-v /proc/<pid>/ns:/target-ns:ro`.
    pub primary_pid: u32,
    /// Host path of the materialized `outrig-enter`, bound read-only.
    pub helper_host: PathBuf,
    /// `HOME` for the payload, from [`Container::home_dir`]. The image's own
    /// `HOME` is a home for the user the image expects to run as -- usually
    /// root, and `/root` is `0700` -- while the payload runs as the session
    /// user. Nothing else corrects it: an entrypoint host skips the exec that
    /// carries `HOME` everywhere else.
    pub payload_home: String,
}

impl PrimaryView {
    /// Join the namespaces of `primary_container`, whose init runs as
    /// `primary_pid`, using the `outrig-enter` helper at `helper_host`, with
    /// the payload's `HOME` set to `payload_home` -- [`Container::home_dir`]
    /// of the primary, so the two placements name the same directory.
    pub fn new(
        primary_container: impl Into<String>,
        primary_pid: u32,
        helper_host: impl Into<PathBuf>,
        payload_home: impl Into<String>,
    ) -> Self {
        Self {
            primary_container: primary_container.into(),
            primary_pid,
            helper_host: helper_host.into(),
            payload_home: payload_home.into(),
        }
    }
}

/// Complete inputs for a `podman run`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerLaunchSpec {
    pub workspace: Option<ContainerWorkspace>,
    pub mounts: Vec<ContainerMount>,
    pub capabilities: ContainerCapabilities,
    /// Host device nodes to pass through, one `--device=<path>` each.
    pub devices: Vec<String>,
    /// Whether to apply `--security-opt=no-new-privileges`.
    pub no_new_privileges: bool,
    /// Paths to exclude from podman's default masking, one
    /// `--security-opt=unmask=<path>` each.
    pub unmask: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Set for a `view = "primary"` sidecar; drives the namespace-joining
    /// flags. `None` is every other container.
    pub primary_view: Option<PrimaryView>,
}

/// Hand-written rather than derived so that `no_new_privileges` defaults to
/// `true` -- a derived `Default` would hand back `false` and silently drop the
/// hardening flag from every caller that starts from `::default()`.
impl Default for ContainerLaunchSpec {
    fn default() -> Self {
        Self {
            workspace: None,
            mounts: Vec::new(),
            capabilities: ContainerCapabilities::default(),
            devices: Vec::new(),
            no_new_privileges: true,
            unmask: Vec::new(),
            labels: BTreeMap::new(),
            primary_view: None,
        }
    }
}

impl ContainerLaunchSpec {
    pub fn workspace(host: impl Into<PathBuf>, container: impl Into<PathBuf>) -> Self {
        Self {
            workspace: Some(ContainerWorkspace::new(
                host,
                container,
                MountAccess::ReadWrite,
            )),
            ..Self::default()
        }
    }
}

/// Complete inputs for a `podman create` + `podman init`, the pair
/// [`Container::create_initialized`] runs. One struct rather than a parameter
/// list because this call has already grown a parameter once, and every knob
/// podman's create step accepts but its run step does not lands here.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ContainerCreateOptions {
    /// Image to create the container from.
    pub image: ImageTag,
    /// Mounts, capabilities, labels -- everything a `podman run` would take.
    pub launch: ContainerLaunchSpec,
    /// Container name, also the handle's identity for tracking and teardown.
    pub name: String,
    /// Where podman's own stdout/stderr is recorded, if anywhere.
    pub transcript: Option<Transcript>,
    /// Becomes `--env` flags on the create. There is no later exec to carry
    /// them, so an entrypoint-stdio server's environment has to be baked in
    /// here.
    pub env: BTreeMap<String, String>,
    /// Bakes the interceptor's loopback resolver in via `--dns`. The
    /// exec-based resolv.conf install is impossible before start.
    pub intercept_dns: bool,
    /// Trailing argv the image's `ENTRYPOINT` receives.
    pub args: Vec<String>,
}

impl ContainerCreateOptions {
    /// Create `image` as a container named `name`, applying `launch`. The
    /// remaining knobs default to empty / off; `with_*` sets them.
    pub fn new(image: ImageTag, launch: ContainerLaunchSpec, name: impl Into<String>) -> Self {
        Self {
            image,
            launch,
            name: name.into(),
            transcript: None,
            env: BTreeMap::new(),
            intercept_dns: false,
            args: Vec::new(),
        }
    }

    /// Record podman's output to `transcript`. Takes an `Option` rather than a
    /// bare `Transcript` because every producer has one -- as
    /// [`Container::start_named`]'s own parameter does.
    pub fn with_transcript(mut self, transcript: Option<Transcript>) -> Self {
        self.transcript = transcript;
        self
    }

    /// Set the environment baked into the create.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Point the container's resolver at the interceptor.
    pub fn with_intercept_dns(mut self, intercept_dns: bool) -> Self {
        self.intercept_dns = intercept_dns;
        self
    }

    /// Set the trailing argv for the image's `ENTRYPOINT`.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}

/// Everything an exec takes besides its argv -- the counterpart of
/// [`ContainerCreateOptions`] on the `podman exec` path, and a struct for the
/// same reason: the environment was the only knob until the working directory
/// joined it, and each further one would otherwise be another parameter on
/// four published methods.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExecOptions {
    /// Added to the environment podman already sets up (`HOME` plus the
    /// mapped user and group). `BTreeMap` order makes the argv deterministic.
    pub env: BTreeMap<String, String>,
    /// Becomes `--workdir <path>`. `None` emits no flag, leaving the
    /// container's configured working directory -- which is the image's
    /// `WORKDIR` only when the launch did not override it. A workspace-backed
    /// launch does override it: the run sets `-w` to the workspace's container
    /// path, so an unset exec runs in the *workspace*, on the host-mounted
    /// checkout. This is what every exec did before the
    /// knob existed; set it explicitly if a relative or destructive command
    /// must not land there.
    pub workdir: Option<PathBuf>,
}

impl ExecOptions {
    /// An exec that adds nothing to podman's own environment and runs in the
    /// container's configured working directory; `with_*` sets each knob. See
    /// [`ExecOptions::workdir`] for what that directory actually is -- with a
    /// workspace mounted it is the workspace, not the image's `WORKDIR`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the environment added to the exec.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Run the command in `workdir` rather than the container's configured
    /// working directory. The path is the container's, not the host's, and
    /// outrig does not check that it exists -- a missing directory is podman's
    /// to report, and probing for it would cost an extra exec on every call.
    pub fn with_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }
}

/// Primary workspace mount. When present, this also sets `-w`. The session's
/// own container mounts it read-write; sidecars may take a read-only view.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerWorkspace {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

impl ContainerWorkspace {
    /// Mount `host` as the workspace at `container`.
    pub fn new(
        host: impl Into<PathBuf>,
        container: impl Into<PathBuf>,
        access: MountAccess,
    ) -> Self {
        Self {
            host: host.into(),
            container: container.into(),
            access,
        }
    }
}

/// Extra bind mount. These do not affect the container working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerMount {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

impl ContainerMount {
    /// Bind `host` at `container`.
    pub fn new(
        host: impl Into<PathBuf>,
        container: impl Into<PathBuf>,
        access: MountAccess,
    ) -> Self {
        Self {
            host: host.into(),
            container: container.into(),
            access,
        }
    }
}

/// Linux capability policy applied to the container at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContainerCapabilities {
    pub profile: CapabilityProfile,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
}

impl ContainerCapabilities {
    /// `profile` with no explicit per-capability overrides. Assign `cap_drop`
    /// / `cap_add` on the result to add them.
    pub fn new(profile: CapabilityProfile) -> Self {
        Self {
            profile,
            ..Self::default()
        }
    }
}

impl Container {
    /// Start a container running `image`, applying the primary workspace and
    /// extra bind mounts from `launch`. A primary workspace is mounted
    /// read-write and used as the working directory; extra mounts never
    /// affect `-w`.
    pub async fn start(image: &ImageTag, launch: ContainerLaunchSpec) -> Result<Self> {
        let name = format!("outrig-{}", runtime_id());
        Self::start_named(image, launch, name, None).await
    }

    /// Start with a caller-supplied container name. Session setup uses this
    /// so `session.json`, `container.log`, and the podman name all share one
    /// preallocated session id.
    pub async fn start_named(
        image: &ImageTag,
        launch: ContainerLaunchSpec,
        name: String,
        transcript: Option<Transcript>,
    ) -> Result<Self> {
        // Armed before anything is spawned, and the only owner of what podman
        // creates until the `Container` below exists. A `?` or a dropped
        // future in between drops the guard, which removes the container it
        // recorded and untracks the name -- neither of which `Drop for
        // Container` can do, because no `Container` has been constructed yet.
        reject_reserved_labels(&launch.labels)?;
        let reserved = NameGuard::reserve(&name);

        let cmd = build_podman_run_cmd(
            image,
            &name,
            &launch,
            selinux_enforcing().await,
            &reserved.attempt_label(),
        );
        let created = process::run_capture_logged(cmd, "podman", transcript.as_ref()).await?;
        // Before the guard is released, so a creation that will not say what
        // it made is removed by the attempt label it was stamped with rather
        // than left behind under a handle that cannot name it.
        let id = engine_id(&created.stdout)
            .ok_or_else(|| unidentified_container(&name, &created.stdout))?;

        let workspace = match &launch.workspace {
            Some(workspace) => (workspace.host.clone(), workspace.container.clone()),
            None => (PathBuf::new(), PathBuf::new()),
        };
        let attempt = reserved.release();
        Ok(Self::handle(
            EngineIdentity::Owned { name, attempt, id },
            image.clone(),
            workspace,
            transcript,
            false,
        ))
    }

    /// Create and initialize a container without executing its ENTRYPOINT:
    /// `podman create` followed by `podman init`, which materializes the
    /// container process and its namespaces while the entrypoint is held
    /// un-executed until `podman start`. Used for entrypoint-stdio MCP
    /// sidecars so the network interceptor can attach to the initialized
    /// PID before the server can emit a packet.
    ///
    /// A `podman init` that fails to materialize a PID surfaces later through
    /// the interceptor's pid probe. See [`ContainerCreateOptions`] for what
    /// each input does.
    pub async fn create_initialized(options: ContainerCreateOptions) -> Result<Self> {
        // As in start_named. The guard matters more here: an `init` that
        // fails, or a future dropped between `create` and `init`, leaves the
        // created container behind, and it is the guard that removes it. A
        // `create` that failed on a name collision records no id, so the same
        // guard removes nothing.
        reject_reserved_labels(&options.launch.labels)?;
        let reserved = NameGuard::reserve(&options.name);

        let create = build_podman_create_cmd(
            &options,
            selinux_enforcing().await,
            &reserved.attempt_label(),
        );
        // The create's own output carries the engine's id for what it made,
        // which is the only name for this container that cannot later mean
        // something else.
        let created =
            process::run_capture_logged(create, "podman", options.transcript.as_ref()).await?;
        // As in `start_named`, and before `init` as well as before the guard
        // is released: a container this cannot name is not one to go on with.
        let id = engine_id(&created.stdout)
            .ok_or_else(|| unidentified_container(&options.name, &created.stdout))?;
        let init = Cmd::new("podman").arg("init").arg(&options.name);
        process::run_capture_logged(init, "podman", options.transcript.as_ref()).await?;

        let workspace = match &options.launch.workspace {
            Some(workspace) => (workspace.host.clone(), workspace.container.clone()),
            None => (PathBuf::new(), PathBuf::new()),
        };
        let attempt = reserved.release();
        Ok(Self::handle(
            EngineIdentity::Owned {
                name: options.name,
                attempt,
                id,
            },
            options.image,
            workspace,
            options.transcript,
            options.intercept_dns,
        ))
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
        let workspace = match workspace {
            Some((host, container)) => (host.to_path_buf(), container.to_path_buf()),
            None => (PathBuf::new(), PathBuf::new()),
        };
        Self::handle(
            EngineIdentity::Borrowed { name: name.into() },
            image_tag,
            workspace,
            transcript,
            false,
        )
    }

    /// Handle constructor shared by every path that materializes a
    /// [`Container`], so a new field is threaded through one place.
    fn handle(
        engine: EngineIdentity,
        image_tag: ImageTag,
        (host_workspace, container_workspace): (PathBuf, PathBuf),
        transcript: Option<Transcript>,
        dns_preconfigured: bool,
    ) -> Self {
        let (name, attempt, id, ownership) = match engine {
            EngineIdentity::Owned { name, attempt, id } => {
                (name, Some(attempt), Some(id), ContainerOwnership::Owned)
            }
            EngineIdentity::Borrowed { name } => (name, None, None, ContainerOwnership::Attached),
        };
        Self {
            name,
            image_tag,
            host_workspace,
            container_workspace,
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            user_name: None,
            group_name: None,
            transcript,
            ownership,
            dns_preconfigured,
            pid: OnceCell::new(),
            id,
            #[cfg(test)]
            engine_override: Vec::new(),
            #[cfg(test)]
            engine_budget: None,
            attempt,
            disposed: false,
        }
    }

    /// A handle to an owned container that can never be stopped, for tests of
    /// what a caller does with one it could not stop.
    ///
    /// The engine call is pointed at `/bin/false`, so the stop fails the same
    /// way on every machine and without podman having to be installed or to
    /// answer in any particular way. A test that wants a different stop
    /// assigns its own `engine_override`, which replaces this one.
    #[cfg(test)]
    pub(crate) fn unstoppable() -> Self {
        let mut container = Self::handle(
            EngineIdentity::Owned {
                name: "outrig-test-unstoppable".to_string(),
                attempt: "outrig-test-never-created".to_string(),
                // An id no container has. Nothing is ever run against it
                // here, but an owned handle cannot be built without one --
                // which is the point of that type.
                id: engine_id(&[b'0'; PODMAN_ID_LEN]).expect("64 hex digits is an id"),
            },
            ImageTag::new("outrig-test-unstoppable"),
            (PathBuf::new(), PathBuf::new()),
            None,
            false,
        );
        container.engine_override = vec![("stop", Cmd::new("/bin/false"))];
        container
    }

    /// A handle whose stop is a no-op that cannot fail: a *borrowed*
    /// container, which outrig never stops because it never started it. The
    /// clean-unwind counterpart to [`unstoppable`](Self::unstoppable).
    #[cfg(test)]
    pub(crate) fn stops_cleanly() -> Self {
        Self::handle(
            EngineIdentity::Borrowed {
                name: "outrig-test-borrowed".to_string(),
            },
            ImageTag::new("outrig-test-borrowed"),
            (PathBuf::new(), PathBuf::new()),
            None,
            false,
        )
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

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The container's init PID via `podman inspect --format {{.State.Pid}}`.
    /// Errors when the container is not running (`State.Pid == 0`): a created
    /// but unstarted container has no namespaces to join or intercept. Shared
    /// by the user bootstrap, the network interceptor, and `view = "primary"`
    /// sidecars, which each want it during startup -- so the answer is cached
    /// after the first success. It cannot go stale: a running container keeps
    /// its init PID until it stops, and a stopped one is not restarted.
    pub async fn pid(&self) -> Result<u32> {
        if let Some(known) = self.pid.get() {
            return Ok(*known);
        }
        let pid = self.inspect_pid().await?;
        Ok(*self.pid.get_or_init(|| async { pid }).await)
    }

    async fn inspect_pid(&self) -> Result<u32> {
        let output = process::run_capture_logged(
            Cmd::new("podman")
                .args(["inspect", "--format", "{{.State.Pid}}"])
                .arg(&self.name),
            "podman",
            self.transcript.as_ref(),
        )
        .await?;
        let text = String::from_utf8_lossy(&output.stdout);
        let pid = text.trim().parse::<u32>().map_err(|e| {
            OutrigError::Configuration(format!(
                "podman inspect {} returned invalid pid: {e}",
                self.name
            ))
        })?;
        if pid == 0 {
            return Err(OutrigError::Configuration(format!(
                "container {:?} has no running namespaces (not running, and not \
                 materialized by `podman init`)",
                self.name
            )));
        }
        Ok(pid)
    }

    /// Whether the interceptor's resolver was baked in at create time; see
    /// the field doc.
    pub(crate) fn dns_preconfigured(&self) -> bool {
        self.dns_preconfigured
    }

    pub fn image_tag(&self) -> &ImageTag {
        &self.image_tag
    }

    pub fn host_workspace(&self) -> &Path {
        &self.host_workspace
    }

    pub fn container_workspace(&self) -> &Path {
        &self.container_workspace
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn user_name(&self) -> Option<&str> {
        self.user_name.as_deref()
    }

    /// The in-container home directory of the bootstrapped user, or `None`
    /// before [`Container::bootstrap_user`] has run. The same path
    /// [`Container::build_exec_argv`] gives every exec-stdio server as `HOME`,
    /// so a sidecar that takes it from here cannot disagree with them.
    pub fn home_dir(&self) -> Option<String> {
        self.user_name.as_deref().map(userdb::home_dir)
    }

    pub fn group_name(&self) -> Option<&str> {
        self.group_name.as_deref()
    }

    /// Materialize an in-container user+group matching the host UID/GID,
    /// reusing existing entries when present and appending `_` to candidate
    /// names on collision.
    ///
    /// Writes `/etc/passwd` and `/etc/group` from the host, through
    /// descriptors a forked child opened inside the container's namespaces
    /// (see [`namespace`]), so the image needs no `useradd`, `groupadd`, or
    /// `getent`. Probes first: on podman 5.x, `--userns=keep-id` auto-injects
    /// the host UID/GID into both files, so there is frequently nothing to
    /// write.
    ///
    /// Records the resolved names on the struct for
    /// [`Container::exec_stdio`] to reference. Must be called once, after
    /// [`Container::start`], before any host-user-scoped exec.
    pub async fn bootstrap_user(&mut self) -> Result<()> {
        let host_user = userdb::sanitize_name(
            &User::from_uid(Uid::from_raw(self.uid))
                .ok()
                .flatten()
                .map(|u| u.name)
                .unwrap_or_default(),
            &format!("u{}", self.uid),
        );
        let host_group = userdb::sanitize_name(
            &Group::from_gid(Gid::from_raw(self.gid))
                .ok()
                .flatten()
                .map(|g| g.name)
                .unwrap_or_default(),
            &format!("g{}", self.gid),
        );

        let pid = self.pid().await?;
        let db = namespace::open_user_db(pid)
            .map_err(|e| self.bootstrap_failed(e.step.label(), e.io()))?;

        let group_name = self.resolve_or_append(&db, namespace::Db::Group, &host_group)?;
        let user_name = self.resolve_or_append(&db, namespace::Db::Passwd, &host_user)?;

        let home = userdb::home_dir(&user_name);
        namespace::create_home(pid, Path::new(&home), self.uid, self.gid)
            .map_err(|e| self.bootstrap_failed(e.step.label(), e.io()))?;

        self.log_bootstrap(&format!(
            "user {user_name} and group {group_name} ready in {}, written from the host",
            self.name
        ))
        .await;

        self.user_name = Some(user_name);
        self.group_name = Some(group_name);
        Ok(())
    }

    /// The name at the host's uid (or gid) in one of the container's
    /// databases, appending a fresh entry when absent. Which id, and how the
    /// entry is spelled, both follow from `which`.
    fn resolve_or_append(
        &self,
        db: &namespace::UserDb,
        which: namespace::Db,
        candidate: &str,
    ) -> Result<String> {
        let text = db
            .read(which)
            .map_err(|e| self.bootstrap_failed(format!("read {}", which.path()), e))?;
        let id = match which {
            namespace::Db::Group => self.gid,
            namespace::Db::Passwd => self.uid,
        };
        if let Some(existing) = userdb::lookup_id(&text, id) {
            return Ok(existing);
        }

        let kind = match which {
            namespace::Db::Group => "group",
            namespace::Db::Passwd => "user",
        };
        let name = userdb::free_name(&text, candidate, BOOTSTRAP_RETRIES)
            .ok_or(OutrigError::BootstrapExhausted { kind })?;
        let line = match which {
            namespace::Db::Group => userdb::group_line(&name, id),
            namespace::Db::Passwd => {
                userdb::passwd_line(&name, id, self.gid, &userdb::home_dir(&name))
            }
        };
        db.append(which, &userdb::append_blob(&text, &line))
            .map_err(|e| self.bootstrap_failed(format!("append to {}", which.path()), e))?;
        Ok(name)
    }

    /// A bootstrap failure, labeled with `step` -- how far the chain into the
    /// container's namespaces got. There is nothing to fall back to, so every
    /// one of them is fatal, and how far it got is the whole diagnostic.
    fn bootstrap_failed(&self, step: impl Into<String>, source: std::io::Error) -> OutrigError {
        OutrigError::BootstrapNamespace {
            container: self.name.clone(),
            step: step.into(),
            source,
        }
    }

    async fn log_bootstrap(&self, line: &str) {
        if let Some(transcript) = &self.transcript {
            let _ = transcript.line("bootstrap", line).await;
        }
    }

    /// Build the argv for a `podman exec -i --user --env HOME ...` invocation
    /// without spawning. `HOME` is always set to the in-container home
    /// directory; entries in `options.env` are forwarded via `--env K=V`
    /// (BTreeMap order makes the resulting argv deterministic), and
    /// `options.workdir` becomes `--workdir <path>` when set. An unset
    /// working directory emits no flag at all, leaving whatever the container
    /// was configured with -- the workspace when the run set `-w`, the image's
    /// `WORKDIR` otherwise.
    ///
    /// Panics if [`Container::bootstrap_user`] has not yet been called --
    /// the user/group don't exist inside the container, so a `--user`-scoped
    /// exec would fail at the podman layer with a less useful message.
    pub(crate) fn build_exec_argv(&self, cmd: &[String], options: &ExecOptions) -> Cmd {
        let user_name = self
            .user_name
            .as_deref()
            .expect("bootstrap_user must be called before build_exec_argv");

        let mut c = Cmd::new("podman")
            .args(["exec", "-i"])
            .arg(format!("--user={}:{}", self.uid, self.gid))
            .arg("--env")
            .arg(format!("HOME={}", userdb::home_dir(user_name)));
        for (k, v) in &options.env {
            c = c.arg("--env").arg(format!("{k}={v}"));
        }
        if let Some(workdir) = &options.workdir {
            c = c.arg("--workdir").arg(workdir);
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
    /// stdio streams piped back to the caller. See [`ExecOptions`] for the
    /// environment and working directory the exec runs under.
    ///
    /// # The returned child is yours, and it is kill-on-drop
    ///
    /// This is the one handle outrig hands out rather than supervises, so the
    /// caller chooses one of three endings for it:
    ///
    /// - **Let it finish.** Hold the handle and `wait()` (or
    ///   `wait_with_output()`) for the command's own exit.
    /// - **Stop it and see it stop.** Hold the handle and `kill().await` --
    ///   or `start_kill()` then `wait()` -- which signals and then reaps.
    /// - **Drop it.** The child is spawned `kill_on_drop(true)`, so a handle
    ///   that goes out of scope -- including one dropped by a cancelled
    ///   future -- SIGKILLs the client. This is the unobserved ending: the
    ///   handle is gone, so nothing is left to `wait()` on, and the reap is
    ///   tokio's orphan queue rather than yours. Prefer one of the two above
    ///   when the outcome matters.
    ///
    /// Outrig does not wait on a child it has given away, which is what makes
    /// the reap the caller's in the first two.
    ///
    /// What is killed is the **host-side `podman exec` client**, not the
    /// process it started: that runs in the container under conmon and
    /// survives its client. Stopping the workload means stopping the
    /// container.
    pub async fn exec_stdio(&self, cmd: &[String], options: &ExecOptions) -> Result<Child> {
        process::spawn_stdio(self.build_exec_argv(cmd, options)).await
    }

    /// [`Self::exec_stdio`], driven to completion: stdout and stderr are
    /// drained concurrently and returned with the exit status. A non-zero
    /// exit is reported in [`Output::status`], not as an error -- the command
    /// ran, and what it made of its arguments is the caller's to judge. A
    /// working directory the container does not have lands here too: podman
    /// exits non-zero and names the path on stderr.
    pub async fn exec_capture(&self, cmd: &[String], options: &ExecOptions) -> Result<Output> {
        process::try_capture(self.build_exec_argv(cmd, options)).await
    }

    pub async fn stop(mut self, grace: Duration) -> Result<()> {
        self.stop_inner(grace).await
    }

    /// The stop this container answers to.
    ///
    /// By id where there is one, because this handle can outlive the name. A
    /// stop that failed or timed out is *kept* for teardown to try again --
    /// and between those two moments the container can go away and free its
    /// name for something else, which a retry aimed at the name would then
    /// stop on this container's behalf. The removal has been scoped to the
    /// attempt label for exactly this reason; the stop had not been.
    ///
    /// `--ignore`: an already-gone container counts as stopped -- an
    /// entrypoint-stdio sidecar exits with its server and `--rm` removes it
    /// before this orderly stop runs. Other stop failures propagate.
    fn stop_cmd(&self, secs: &str) -> Cmd {
        Cmd::new("podman")
            .args(["stop", "--ignore", "-t"])
            .arg(secs)
            .arg(self.id.as_ref().map_or(&*self.name, ContainerId::as_str))
    }

    /// The command to actually run for `real`. Itself outside tests.
    fn engine_call(&self, real: Cmd) -> Cmd {
        #[cfg(test)]
        {
            let rendered = real.render();
            if let Some((_, stand_in)) = self
                .engine_override
                .iter()
                .find(|(fragment, _)| rendered.contains(fragment))
            {
                return stand_in.clone();
            }
        }
        real
    }

    /// How long one bounded engine call gets before it counts as wedged.
    fn engine_budget(&self, real: Duration) -> Duration {
        #[cfg(test)]
        if let Some(budget) = self.engine_budget {
            return budget;
        }
        real
    }

    /// [`stop`](Self::stop), handing the container back if it did not stop.
    ///
    /// `None` means it is gone and the handle with it. `Some` means it is
    /// still running, and carries both the failure and the handle -- because
    /// a container nothing holds has only [`Drop`]'s detached removal left,
    /// which reports nothing and which teardown cannot retry.
    ///
    /// This is the form for a caller compensating for some earlier failure:
    /// the handle comes back whether it is wanted or not, so keeping it is
    /// not something a call site can forget to do.
    pub async fn stop_or_keep(mut self, grace: Duration) -> Option<(OutrigError, Self)> {
        match self.stop_inner(grace).await {
            Ok(()) => None,
            Err(e) => Some((e, self)),
        }
    }

    async fn stop_inner(&mut self, grace: Duration) -> Result<()> {
        if self.ownership == ContainerOwnership::Attached {
            self.disposed = true;
            return Ok(());
        }

        let secs = grace.as_secs().to_string();
        let stop = self.stop_cmd(&secs);
        // Bounded, and not by `-t`: that is how long podman waits for the
        // *container's* processes before it kills them, and says nothing about
        // the client asking for it. A wedged client, or an engine that never
        // answers, would otherwise hold this await forever -- and this await
        // is on the path of every sidecar compensation and every shutdown, so
        // "forever" is the whole session. The budget is the grace the
        // container is owed plus the client's own floor on top of it.
        let stopped = process::try_capture_logged_until(
            self.engine_call(stop.clone()),
            "podman",
            self.transcript.as_ref(),
            // Saturating, because `grace` is a caller's number and
            // `Duration`'s `+` panics on overflow: a library caller passing
            // `Duration::MAX` -- or anything within `MIN_REMOVAL_BUDGET` of it
            // -- would have brought the process down before any cleanup ran.
            // Saturating gives such a caller what they asked for, which is a
            // budget longer than the machine will be up for.
            tokio::time::sleep(self.engine_budget(grace.saturating_add(MIN_REMOVAL_BUDGET))),
        )
        .await;
        match classify_engine_call(&stop, stopped) {
            EngineOutcome::Done => {}
            // Nothing here can tell whether the container stopped, so it is
            // not disposed of and not untracked: the caller keeps a handle to
            // try again through, and `Drop` still has its detached removal.
            EngineOutcome::TimedOut => {
                return Err(OutrigError::Canceled {
                    program: stop.program,
                    argv: stop.args,
                });
            }
            EngineOutcome::Failed(e) => return Err(e),
        }
        // `--rm` in start() makes this redundant on the success path, but
        // run it defensively in case `--rm` got disabled or the daemon
        // failed to honor it. try_capture so a removal that finds nothing
        // doesn't turn into an error.
        //
        // Scoped to the attempt like the detached removals, and for the same
        // reason rather than a weaker version of it: the `podman stop` above
        // has already returned, so with `--rm` the container is gone and the
        // name is free *before* this command resolves it. That interval is
        // short, but "short" is not "absent", and what it costs is someone
        // else's container.
        //
        // Bounded, where it used to be an unbounded await whose result was
        // discarded: `rm -f` SIGKILLs rather than waiting, so a client still
        // running after this is wedged, not working, and `stop` must return
        // regardless. Stopping it cooperatively rather than dropping the
        // future is what makes the client confirmed gone on return -- this is
        // the last thing to touch the container name, and a podman client
        // still holding it is how the next run under that name fails.
        let removal_budget = grace.max(MIN_REMOVAL_BUDGET);
        let removal = removal_cmd(&self.name, self.attempt.as_deref()).cmd;
        let removed = process::try_capture_logged_until(
            self.engine_call(removal.clone()),
            "podman",
            self.transcript.as_ref(),
            tokio::time::sleep(self.engine_budget(removal_budget)),
        )
        .await;
        match classify_engine_call(&removal, removed) {
            EngineOutcome::Done => {}
            // A removal that did not answer has not removed anything that
            // anything here can see. It used to be handed to a detached retry
            // and then reported as a completed stop, which said "gone, and the
            // handle with it" about a container whose state was unknown --
            // and if the detached retries also failed, what was left had no
            // observable owner and nothing orderly coming for it. Reported
            // instead, with the handle intact: the caller decides whether to
            // try again or to abandon it to teardown, and `Drop` still has the
            // detached removal if the handle is let go.
            EngineOutcome::TimedOut => {
                return Err(OutrigError::Canceled {
                    program: removal.program,
                    argv: removal.args,
                });
            }
            // Neither disposed nor untracked, so the caller keeps something to
            // try again through and `Drop` still has its detached removal to
            // fall back on. A stop that says it worked is how a leak becomes
            // nobody's.
            EngineOutcome::Failed(e) => return Err(e),
        }
        untrack(&self.name);
        self.disposed = true;
        Ok(())
    }

    /// The session transcript podman commands are logged to, if any. Cloned
    /// so mid-session container starts (e.g. `/sidecar add`) can log into
    /// the same `container.log`.
    pub fn transcript(&self) -> Option<Transcript> {
        self.transcript.clone()
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        if self.disposed || self.ownership == ContainerOwnership::Attached {
            return;
        }
        removal_cmd(&self.name, self.attempt.as_deref()).detach();
        untrack(&self.name);
    }
}

/// Ownership of a container this call is creating, covering the window
/// between the name being chosen and a [`Container`] existing to own it.
///
/// [`Drop`] for `Container` cannot cover that window -- there is no
/// `Container` yet -- and the panic hook only fires on a panic, so a cancelled
/// or failed create used to leave the name in `TRACKED` forever and, if podman
/// had already made the container, the container running with nothing that
/// would remove it.
///
/// # Why this removes by label and not by name
///
/// A name is a request, not a claim. `podman run --name N` fails when N is
/// already in use, and that is an ordinary outcome -- a caller reusing a name,
/// a previous session that outlived its record, a container made by hand.
/// Removing N on the way out of that failure would destroy **someone else's
/// container**, which is a far worse outcome than the leak this guard exists
/// to prevent. The same is true under cancellation, where outrig never learns
/// why the command ended, and of a cleanup still in flight when the caller
/// retries under the same name.
///
/// So each attempt stamps a fresh random [`ATTEMPT_LABEL`] onto the container
/// it asks for, and the guard removes by that label. A run that failed on a
/// collision created nothing carrying the label, so it removes nothing -- the
/// distinction falls out of the mechanism rather than being a case anyone has
/// to remember.
///
/// A label rather than a `--cidfile` because a label exists from the instant
/// the container does: it is part of the creation request, so there is no
/// interval in which podman has registered a container the guard cannot yet
/// name. A cidfile is written *after* creation, and a cancellation landing in
/// between would leave behind exactly the container this guard is for.
struct NameGuard {
    /// `None` once [`Self::release`] has handed the obligation on.
    name: Option<String>,
    /// Identifies the container *this attempt* asked podman to create, and
    /// nothing else on the machine.
    attempt: String,
}

impl NameGuard {
    /// Reserve `name`, registering it with `TRACKED` so the panic hook sees it
    /// as well.
    fn reserve(name: &str) -> Self {
        track(name);
        Self {
            name: Some(name.to_string()),
            attempt: attempt_token(),
        }
    }

    /// The `--label` this attempt's container must carry for the guard to
    /// recognize it as its own.
    fn attempt_label(&self) -> String {
        format!("{ATTEMPT_LABEL}={}", self.attempt)
    }

    /// Hand the container to a constructed [`Container`], whose own `Drop`
    /// covers it from here on, and give it the attempt token so that its
    /// removals can be scoped the same way this one's are.
    fn release(mut self) -> String {
        self.name = None;
        std::mem::take(&mut self.attempt)
    }
}

impl Drop for NameGuard {
    fn drop(&mut self) {
        let Some(name) = self.name.take() else {
            return;
        };
        removal_cmd(&name, Some(&self.attempt)).detach();
        untrack(&name);
    }
}

/// The removal for a container this process created: by its attempt label
/// where that is known, and by name only where it is not.
///
/// One selector for every owned removal, rather than a filtered one in the
/// guard and a bare one everywhere else. The distinction bites hardest on a
/// removal that runs *later* than the call that owed it -- `Container::stop`'s
/// fallback after a spent budget, and `Drop for Container`, both of which are
/// detached and resolve their target whenever the engine gets to them. By then
/// the container can be gone and its name can belong to a replacement: a name
/// is a request, not a claim, and the label is the only part of the request
/// that is this attempt's alone. See [`NameGuard`] for the argument in full.
fn removal_cmd(name: &str, attempt: Option<&str>) -> Removal {
    match attempt {
        Some(token) => Removal {
            cmd: Cmd::new("podman")
                .args(["rm", "-f", "--filter"])
                .arg(format!("label={ATTEMPT_LABEL}={token}")),
            // The label is this attempt's alone, so re-issuing the removal
            // later can still only reach what this attempt made.
            reissue: Reissue::Safe,
        },
        // An attached container has no attempt of outrig's behind it, so
        // there is nothing to scope to. Nothing that reaches here removes
        // one: `Drop` and `stop` both return early for them.
        None => Removal {
            cmd: Cmd::new("podman").args(["rm", "-f"]).arg(name),
            reissue: Reissue::Once,
        },
    }
}

/// What a removal that has been run leaves for its caller.
enum EngineOutcome {
    /// It did what was asked. For a removal, that includes matching nothing:
    /// the filter form exits zero when `--rm` has already done the work.
    Done,
    /// The engine did not answer inside the budget. Nothing is known about the
    /// container, so the obligation is handed to a detached retry rather than
    /// dropped -- and that retry is what lets the handle be disposed of.
    TimedOut,
    /// It ran and did not work, or could not be run at all. Either way the
    /// container may still be there, under a name still spoken for.
    Failed(OutrigError),
}

/// Read a removal's outcome, the distinction being whether anything is still
/// owed afterwards.
///
/// A non-zero exit is a failure and was not treated as one: only the timeout
/// was, so podman refusing the removal -- a storage error, a container the
/// engine will not let go of -- read as success, and the handle was disposed
/// of on the strength of it. The filter form matches nothing when `--rm` has
/// already done the work and exits zero for that (measured against podman
/// 4.9.3), so a non-zero exit here is the engine saying it could not do what
/// was asked, not that there was nothing to do.
fn classify_engine_call(cmd: &Cmd, outcome: Result<Output>) -> EngineOutcome {
    match outcome {
        Ok(output) if output.status.success() => EngineOutcome::Done,
        Ok(output) => EngineOutcome::Failed(OutrigError::Process {
            program: cmd.program,
            argv: cmd.args.clone(),
            exit_code: output.status.code(),
            stderr_tail: process::tail_string(&output.stderr, process::STDERR_TAIL_LIMIT),
        }),
        Err(OutrigError::Canceled { .. }) => EngineOutcome::TimedOut,
        // It could not be run at all, which says nothing about whether the
        // container is gone.
        Err(e) => EngineOutcome::Failed(e),
    }
}

/// Everything podman knows one container by, in the two shapes there are.
///
/// The three names are not interchangeable and the difference is the whole
/// point: the *name* is what was asked for and can be granted again, the
/// *attempt* label is stamped on one request and scopes a removal to what that
/// request made, and the *id* is the engine's own and is never handed out
/// twice.
///
/// An enum rather than three fields with three `Option`s, so that "a container
/// outrig made, whose id it did not get" cannot be built at all. That state is
/// what makes a stop fall back to the name, and a handle kept for a retry can
/// outlive its name -- so the compiler refuses it here instead of a test
/// hoping to catch it at the other end.
enum EngineIdentity {
    /// One outrig created and is responsible for stopping and removing.
    Owned {
        name: String,
        attempt: String,
        id: ContainerId,
    },
    /// One outrig only borrowed: it neither stops nor removes it, and podman
    /// never told it an id.
    Borrowed { name: String },
}

/// The container id `podman create` or `podman run -d` printed, if what it
/// printed is one.
///
/// Podman writes the full 64-character hex id and nothing else on success, so
/// that is what is required. Taking any single token instead -- which this
/// did -- makes whatever a wrapper script, a shim, or an engine with a
/// different output format happens to print into a *container selector*, and
/// that selector is then what every stop this handle issues names.
///
/// `None` is not a fallback to the name here: the callers refuse to build a
/// handle without an id, so an unreadable answer fails the creation while the
/// guard that removes by attempt label is still armed. Only a container outrig
/// borrowed rather than made has no id, and it is never stopped.
fn engine_id(stdout: &[u8]) -> Option<ContainerId> {
    let printed = std::str::from_utf8(stdout).ok()?.trim();
    (printed.len() == PODMAN_ID_LEN && printed.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| ContainerId(printed.to_string()))
}

/// Podman's own id for one container, and only ever that.
///
/// A newtype with no `Default`, no `From<String>` and a private field, so the
/// single way to have one is [`engine_id`] reading it out of what the engine
/// printed. Without that, "the id" is a `String` like any other and every
/// fallback that produces one -- an empty default, the container's name --
/// type-checks, which is how the name this exists to avoid gets back in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerId(String);

impl ContainerId {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a creation that would not say what it made reports.
fn unidentified_container(name: &str, stdout: &[u8]) -> OutrigError {
    OutrigError::Configuration(format!(
        "podman did not report a container id for {name:?}; it printed {:?}, and \
         a handle that cannot name what it made can only name it by a name that \
         may later be something else's",
        process::tail_string(stdout, PODMAN_ID_LEN * 2)
    ))
}

/// A removal, and whether issuing it a second time could reach something else.
struct Removal {
    cmd: Cmd,
    reissue: Reissue,
}

impl Removal {
    /// Hand the removal to the supervisor, with the reissue policy its
    /// selector earns.
    fn detach(self) {
        crate::supervise::detach_cleanup(self.cmd, self.reissue);
    }
}

/// Reject a caller trying to set outrig's own bookkeeping label.
///
/// podman takes the last `--label` for a key, so a caller supplying this one
/// would replace the value the guard removes by and quietly disable it. The
/// internal label is also emitted *after* the caller's as defense in depth;
/// this is the half that says so rather than silently winning.
fn reject_reserved_labels(labels: &BTreeMap<String, String>) -> Result<()> {
    if labels.contains_key(ATTEMPT_LABEL) {
        return Err(OutrigError::Configuration(format!(
            "`{ATTEMPT_LABEL}` is reserved: outrig sets it per container-start attempt \
             so that a cancelled start removes what it created and nothing else"
        )));
    }
    Ok(())
}

/// A value no other container on the machine carries, so a removal scoped to
/// it cannot reach anything this process did not ask for.
fn attempt_token() -> String {
    use rand::Rng;

    let mut buf = [0_u8; 16];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Best-effort, fire-and-forget `podman rm -f <name>`. Public form of the
/// `Drop`/panic-hook sweeper for callers that must reap a container they do
/// not hold a `Container` handle for (e.g. the session watcher reaping
/// sidecars after the primary dies out from under outrig).
pub fn force_remove_detached(name: &str) {
    spawn_detached_rm(name);
}

/// Best-effort `podman rm -f <name>` with stdio nulled. Synchronous,
/// detached, requires no tokio runtime -- safe from `Drop` and panic hooks.
/// Through [`crate::supervise`], so the `podman rm` it starts is reaped
/// rather than left a zombie for the life of the process.
fn spawn_detached_rm(name: &str) {
    // By name, so issued once: the name can belong to a replacement by the
    // time a retry would land, and removing that is worse than the leak.
    crate::supervise::detach_cleanup(
        Cmd::new("podman").args(["rm", "-f"]).arg(name),
        Reissue::Once,
    );
}

fn build_podman_run_cmd(
    image: &ImageTag,
    name: &str,
    launch: &ContainerLaunchSpec,
    selinux: bool,
    attempt_label: &str,
) -> Cmd {
    let cmd = Cmd::new("podman")
        .args(["run", "-d", "--rm", "--name"])
        .arg(name);
    // After the caller's labels, so podman's last-wins parsing cannot let one
    // of theirs displace the value cleanup removes by.
    append_launch_flags(cmd, launch, selinux)
        .arg("--label")
        .arg(attempt_label)
        .arg(image.as_str())
        .args(["sleep", "infinity"])
}

/// `podman create` argv for an entrypoint-stdio container: the image's
/// ENTRYPOINT is the process, `--interactive` so stdin stays open for the
/// later `podman start --attach --interactive`, and `--rm` so container
/// lifetime equals server lifetime. `env` becomes `--env` flags because there
/// is no later exec to carry it; `intercept_dns` bakes the interceptor's
/// resolver in via `--dns` for the same reason.
///
/// `args` is the trailing argv, after the image ref. podman appends it to an
/// exec-form ENTRYPOINT and *replaces* CMD, so it is for images whose server
/// is an ENTRYPOINT. Empty `args` emits nothing, leaving the argument vector
/// byte-identical to the pre-`args` one.
fn build_podman_create_cmd(
    options: &ContainerCreateOptions,
    selinux: bool,
    attempt_label: &str,
) -> Cmd {
    let mut cmd = Cmd::new("podman")
        .args(["create", "--name"])
        .arg(&options.name);
    cmd = append_launch_flags(cmd, &options.launch, selinux);

    if options.intercept_dns {
        cmd = cmd
            .args(["--dns", crate::network::INTERCEPT_DNS_NAMESERVER])
            .args(["--dns-option", crate::network::INTERCEPT_DNS_OPTION]);
    }
    // `HOME` first, so a configured `env` entry of the same name wins: podman
    // takes the last `--env` for a key. A `view = "primary"` payload is the
    // only container process that gets one from here -- everything else is
    // either exec'd (`build_exec_argv` carries it) or runs as the user its own
    // image expects.
    if let Some(pv) = &options.launch.primary_view {
        cmd = cmd.arg("--env").arg(format!("HOME={}", pv.payload_home));
    }
    for (k, v) in &options.env {
        cmd = cmd.arg("--env").arg(format!("{k}={v}"));
    }

    // After the caller's labels; see `build_podman_run_cmd`.
    cmd.arg("--label")
        .arg(attempt_label)
        .args(["--interactive", "--rm"])
        .arg(options.image.as_str())
        .args(&options.args)
}

/// Flags shared by `podman run` and `podman create`: labels, workspace and
/// extra bind mounts, keep-id, workspace workdir, capability policy, device
/// passthrough, and the hardening tail. `--security-opt=no-new-privileges` is
/// part of that tail only when the launch spec keeps it, and each `unmask`
/// entry follows it as a second `--security-opt`.
fn append_launch_flags(mut cmd: Cmd, launch: &ContainerLaunchSpec, selinux: bool) -> Cmd {
    for (key, value) in &launch.labels {
        cmd = cmd.arg("--label").arg(format!("{key}={value}"));
    }

    if let Some(workspace) = &launch.workspace {
        cmd = append_bind_mount(
            cmd,
            &workspace.host,
            &workspace.container,
            workspace.access,
            selinux,
        );
    }
    for mount in &launch.mounts {
        cmd = append_bind_mount(cmd, &mount.host, &mount.container, mount.access, selinux);
    }

    // A `view = "primary"` sidecar binds the primary's nsfs directory and the
    // launcher. Both use a plain `:ro` and bypass `append_bind_mount`: podman's
    // SELinux `,Z` relabels the source, which is wrong (and fails) for
    // `/proc/<pid>/ns`. Bind the nsfs *directory*, not the file -- podman forces
    // `MS_REC` on a `-v`, which nsfs rejects on a single file.
    // ...then the userns: a primary-view sidecar must be in the user namespace
    // that *owns* the primary's mount namespace, so `setns` is permitted; every
    // other container keeps `--userns=keep-id`. Emitted together with the binds
    // since nothing goes between them.
    match &launch.primary_view {
        Some(pv) => {
            cmd = cmd.arg("-v").arg(format!(
                "/proc/{}/ns:{PRIMARY_VIEW_NS_MOUNT}:ro",
                pv.primary_pid
            ));
            cmd = cmd.arg("-v").arg(format!(
                "{}:{PRIMARY_VIEW_HELPER_MOUNT}:ro",
                pv.helper_host.display()
            ));
            cmd = cmd.arg(format!("--userns=container:{}", pv.primary_container));
        }
        None => cmd = cmd.arg("--userns=keep-id"),
    }
    if let Some(workspace) = &launch.workspace {
        cmd = cmd.arg("-w").arg(&workspace.container);
    }

    cmd = append_capability_flags(cmd, &launch.capabilities);
    // `open_tree`/`setns(CLONE_NEWNS)` need CAP_SYS_ADMIN; opening the target's
    // nsfs file needs CAP_SYS_PTRACE. Scoped to the rootless user namespace.
    if launch.primary_view.is_some() {
        cmd = cmd.arg("--cap-add=SYS_ADMIN").arg("--cap-add=SYS_PTRACE");
    }
    for device in &launch.devices {
        cmd = cmd.arg(format!("--device={device}"));
    }
    if launch.no_new_privileges {
        cmd = cmd.arg("--security-opt=no-new-privileges");
    }
    // One flag per entry rather than podman's colon-joined form: the lowering
    // stays trivial and a failed launch's argv stays readable.
    for path in &launch.unmask {
        cmd = cmd.arg(format!("--security-opt=unmask={path}"));
    }
    if launch.primary_view.is_some() {
        cmd = cmd.arg("--entrypoint").arg(PRIMARY_VIEW_HELPER_MOUNT);
    }
    cmd.arg("--pull=never")
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

fn runtime_id() -> String {
    use jiff::Zoned;
    use rand::Rng;

    let ts = Zoned::now()
        .with_time_zone(jiff::tz::TimeZone::UTC)
        .strftime("%Y%m%dT%H%M%S");
    let mut buf = [0_u8; 2];
    rand::rng().fill_bytes(&mut buf);
    format!("{ts}-{:02x}{:02x}", buf[0], buf[1])
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
        image_tag: ImageTag::new(image),
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

    /// The stop names the container the engine made, not the name it was
    /// asked for. A handle kept for a retry -- which is what a failed stop
    /// leaves behind -- can outlive the name: the container goes away, the
    /// name is free, and something else takes it before teardown gets around
    /// to trying again. A retry aimed at the name stops *that* container. The
    /// removal has been scoped to the attempt label all along for the same
    /// reason; the stop had not been.
    #[test]
    fn a_stop_names_the_container_and_not_a_name_something_else_can_have() {
        let id = "2f8b1c0d".repeat(8);
        let mut container = Container::unstoppable();
        container.name = "outrig-a-name-that-can-come-round-again".to_string();
        container.id = engine_id(id.as_bytes());

        let asked = container.stop_cmd("2").render();
        assert!(
            asked.contains(&id),
            "the stop has to name the engine's id: {asked}"
        );
        assert!(
            !asked.contains("come-round-again"),
            "and not a name that can come round again: {asked}"
        );

        // A container outrig only borrowed has no id -- and never reaches a
        // stop either, since stopping one is a no-op it returns early from.
        let borrowed = Container::stops_cleanly();
        assert_eq!(borrowed.id, None);
    }

    /// An id is what podman prints for a `create`, and nothing else counts as
    /// one. Taking any single token makes whatever a wrapper, a shim or an
    /// engine with another output format prints into a *container selector* --
    /// and that selector is what every stop this handle issues would name.
    #[test]
    fn an_engine_id_is_taken_only_from_output_that_is_one() {
        let real = "8a29737190d0a814b5930e4a5eb1a1dd4dbf0da72b40ad6ac7d6fd0f0bbf3ca1";
        assert_eq!(real.len(), PODMAN_ID_LEN);
        assert_eq!(
            engine_id(real.as_bytes()).as_ref().map(ContainerId::as_str),
            Some(real)
        );
        // Podman ends it with a newline, and a stray blank line is still it.
        assert_eq!(
            engine_id(format!("{real}\n\n").as_bytes())
                .as_ref()
                .map(ContainerId::as_str),
            Some(real)
        );

        for not_an_id in [
            // Nothing at all.
            String::new(),
            "   \n".to_string(),
            // A token, but not an id: this is what a wrapper script or an
            // engine with its own output format gets to inject.
            "some-other-container".to_string(),
            "--all".to_string(),
            // A short id. Podman would accept it as a selector, which is
            // exactly why it is not accepted as *this* one's identity.
            real[..12].to_string(),
            // Right length, wrong alphabet.
            "z".repeat(PODMAN_ID_LEN),
            // Right shape, but two of them.
            format!("{real} {real}"),
        ] {
            assert_eq!(
                engine_id(not_an_id.as_bytes()),
                None,
                "not an id: {not_an_id:?}"
            );
        }
        // Not text at all.
        assert_eq!(engine_id(&[0xff, 0xfe]), None);
    }

    /// A stop whose client never returns is given up on, and the container
    /// comes back. `-t` bounds how long podman waits for the container's
    /// *processes*; it says nothing about the client asking for it, so a
    /// wedged client or an engine that never answers used to hold this await
    /// for the rest of the session -- and this await is on the path of every
    /// sidecar compensation and every shutdown.
    #[tokio::test]
    async fn a_stop_client_that_never_returns_is_given_up_on() {
        let mut container = Container::unstoppable();
        // Ten seconds, against a budget of one: long enough to be wedged
        // relative to the deadline under test, short enough that losing that
        // deadline costs the suite ten seconds and a failed assertion rather
        // than a hang. Real time, not a paused clock: a bounded call waiting
        // on a child leaves the runtime idle, and a paused clock fires every
        // deadline in the test at once, including the one belonging to the
        // call this is not about.
        container.engine_override = vec![("stop", Cmd::new("/bin/sleep").arg("10"))];
        container.engine_budget = Some(Duration::from_secs(1));

        let (why, kept) = container
            .stop_or_keep(Duration::from_secs(2))
            .await
            .expect("a stop that never answered is not a stop");
        let OutrigError::Canceled { argv, .. } = &why else {
            panic!("a wedged client is given up on, not failed: {why:?}");
        };
        assert_eq!(
            argv.first().map(|a| a.to_string_lossy().into_owned()),
            Some("stop".to_string()),
            "and it is the *stop* that was given up on: {argv:?}"
        );
        // Nothing here can tell whether the container stopped, so the handle
        // is the caller's to try again through.
        assert!(
            kept.stop(Duration::from_secs(1)).await.is_err(),
            "the handle has to still be the container"
        );
    }

    /// A removal that never answers is not a removal either. It used to be
    /// handed to a detached retry and then reported as a completed stop --
    /// "gone, and the handle with it" about a container whose state was
    /// unknown, with nothing observable left if those retries also failed.
    #[tokio::test]
    async fn a_removal_that_never_returns_does_not_count_as_a_stop() {
        let mut container = Container::unstoppable();
        // The stop is held still so the removal is reached at all, and so the
        // test needs no engine of its own.
        container.engine_override = vec![
            ("stop", Cmd::new("/bin/true")),
            ("rm", Cmd::new("/bin/sleep").arg("10")),
        ];
        container.engine_budget = Some(Duration::from_secs(1));

        let (why, _kept) = container
            .stop_or_keep(Duration::from_secs(2))
            .await
            .expect("an unconfirmed removal is not a completed stop");
        let OutrigError::Canceled { argv, .. } = &why else {
            panic!("a wedged removal is given up on, not failed: {why:?}");
        };
        assert_eq!(
            argv.first().map(|a| a.to_string_lossy().into_owned()),
            Some("rm".to_string()),
            "and it is the *removal* that was given up on: {argv:?}"
        );
    }

    /// A removal that ran and failed is not a removal. Reading every outcome
    /// but a timeout as success is how a container the engine refused to
    /// remove became nobody's: the handle was disposed of on the strength of
    /// it, so nothing retried and `Drop` had nothing left to do either.
    #[test]
    fn a_removal_that_did_not_work_is_not_read_as_success() {
        use std::os::unix::process::ExitStatusExt;

        let cmd = Cmd::new("podman").args(["rm", "-f"]);
        let output = |code: i32, stderr: &str| {
            Ok(Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            })
        };

        // Nothing matched the filter, which is what `--rm` having already done
        // the work looks like.
        assert!(matches!(
            classify_engine_call(&cmd, output(0, "")),
            EngineOutcome::Done
        ));

        // The engine ran it and refused.
        let failed = classify_engine_call(&cmd, output(125, "Error: container is in use"));
        let EngineOutcome::Failed(e) = failed else {
            panic!("a non-zero removal must be reported");
        };
        assert!(
            e.to_string().contains("container is in use"),
            "and say what the engine said: {e}"
        );

        // It could not be run at all, which says nothing about the container.
        let failed =
            classify_engine_call(&cmd, Err(OutrigError::Configuration("no podman".into())));
        assert!(
            matches!(failed, EngineOutcome::Failed(_)),
            "a removal that never ran has not removed anything"
        );

        // The one outcome that is still owed to something else: the detached
        // retry the caller hands over, which is what lets it dispose.
        let timed_out = classify_engine_call(
            &cmd,
            Err(OutrigError::Canceled {
                program: "podman",
                argv: Vec::new(),
            }),
        );
        assert!(matches!(timed_out, EngineOutcome::TimedOut));
    }

    /// A stop that failed hands the container back. Every caller of this is
    /// compensating for an earlier failure, and one that dropped the handle
    /// would leave a container running with nothing that can retry stopping
    /// it -- only `Drop`'s detached removal, which reports nothing.
    #[tokio::test]
    async fn a_container_that_will_not_stop_comes_back_to_its_caller() {
        let container = Container::unstoppable();

        let kept = container
            .stop_or_keep(Duration::from_secs(1))
            .await
            .expect("a stop that cannot work has to hand the container back");
        let (_, kept) = kept;

        // And what comes back is usable: the point of keeping it is that
        // teardown can try the same container again.
        assert!(
            kept.stop(Duration::from_secs(1)).await.is_err(),
            "the handle has to be the container, not a husk of one"
        );
    }

    fn argv(cmd: Cmd) -> Vec<String> {
        std::iter::once(cmd.program.to_string())
            .chain(
                cmd.args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect()
    }

    /// The removal a created container owes names *that* container, and a
    /// name is not a name of it: the same string can belong to a replacement
    /// by the time a detached removal runs.
    #[test]
    fn a_removal_for_a_created_container_is_scoped_to_its_attempt() {
        assert_eq!(
            argv(removal_cmd("outrig-test", Some("testtoken")).cmd),
            vec![
                "podman",
                "rm",
                "-f",
                "--filter",
                "label=org.outrig.attempt=testtoken",
            ],
            "the container's own name must not appear: it is what a \
             replacement would share with it"
        );
        assert_eq!(
            removal_cmd("outrig-test", Some("testtoken")).reissue,
            Reissue::Safe,
            "a per-attempt label still means this attempt however late it is used"
        );
    }

    /// Without an attempt there is nothing to scope to, so the name is all
    /// there is. Nothing that removes reaches this: `Drop` and `stop` both
    /// return early for an attached container, which is the only kind that
    /// has no attempt behind it.
    #[test]
    fn a_removal_without_an_attempt_falls_back_to_the_name() {
        assert_eq!(
            argv(removal_cmd("outrig-test", None).cmd),
            vec!["podman", "rm", "-f", "outrig-test"]
        );
        assert_eq!(
            removal_cmd("outrig-test", None).reissue,
            Reissue::Once,
            "a bare name may be reused, so this one must never be re-issued"
        );
    }

    #[test]
    fn podman_run_args_include_workspace_then_extra_mounts() {
        let launch = ContainerLaunchSpec {
            workspace: Some(ContainerWorkspace {
                host: "/host/repo".into(),
                container: "/workspace".into(),
                access: MountAccess::ReadWrite,
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
            labels: BTreeMap::new(),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_include_labels_and_ro_workspace() {
        let launch = ContainerLaunchSpec {
            workspace: Some(ContainerWorkspace {
                host: "/host/repo".into(),
                container: "/workspace".into(),
                access: MountAccess::ReadOnly,
            }),
            mounts: Vec::new(),
            capabilities: ContainerCapabilities::default(),
            labels: BTreeMap::from([
                (
                    LABEL_SESSION.to_string(),
                    "20260711T000000-abcd".to_string(),
                ),
                (LABEL_SIDECAR.to_string(), "tools".to_string()),
            ]),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-20260711T000000-abcd-tools",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "run",
                "-d",
                "--rm",
                "--name",
                "outrig-20260711T000000-abcd-tools",
                "--label",
                "org.outrig.session=20260711T000000-abcd",
                "--label",
                "org.outrig.sidecar=tools",
                "-v",
                "/host/repo:/workspace:ro",
                "--userns=keep-id",
                "-w",
                "/workspace",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
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
            labels: BTreeMap::new(),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            true,
            "org.outrig.attempt=testtoken",
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
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_create_args_hold_entrypoint_with_env_and_dns() {
        let launch = ContainerLaunchSpec {
            workspace: None,
            mounts: Vec::new(),
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::NoNetRaw,
                cap_drop: Vec::new(),
                cap_add: Vec::new(),
            },
            labels: BTreeMap::from([
                (
                    LABEL_SESSION.to_string(),
                    "20260712T000000-abcd".to_string(),
                ),
                (LABEL_SIDECAR.to_string(), "fetch".to_string()),
            ]),
            ..Default::default()
        };
        let env = BTreeMap::from([
            ("A_FIRST".to_string(), "1".to_string()),
            ("TOKEN".to_string(), "secret value".to_string()),
        ]);

        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(
                ImageTag::new("ghcr.io/example/mcp-fetch:2"),
                launch,
                "outrig-20260712T000000-abcd-fetch",
            )
            .with_env(env)
            .with_intercept_dns(true),
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-20260712T000000-abcd-fetch",
                "--label",
                "org.outrig.session=20260712T000000-abcd",
                "--label",
                "org.outrig.sidecar=fetch",
                "--userns=keep-id",
                "--cap-drop=NET_RAW",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--dns",
                "127.0.0.1",
                "--dns-option",
                "ndots:0",
                "--env",
                "A_FIRST=1",
                "--env",
                "TOKEN=secret value",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "ghcr.io/example/mcp-fetch:2",
            ]
        );
    }

    #[test]
    fn podman_create_args_omit_dns_and_env_when_unused() {
        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(
                ImageTag::new("local:test"),
                ContainerLaunchSpec::default(),
                "outrig-test-fetch",
            ),
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-test-fetch",
                "--userns=keep-id",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "local:test",
            ]
        );
    }

    /// `args` is the trailing argv: strictly after the image ref, so podman
    /// hands it to the ENTRYPOINT rather than reading it as a flag.
    #[test]
    fn podman_create_args_append_entrypoint_argv_after_the_image() {
        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(
                ImageTag::new("docker.io/mcp/filesystem:latest"),
                ContainerLaunchSpec::default(),
                "outrig-test-fs",
            )
            .with_env(BTreeMap::from([("MARKER".to_string(), "1".to_string())]))
            .with_args(vec!["/workspace".to_string(), "--read-only".to_string()]),
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-test-fs",
                "--userns=keep-id",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--env",
                "MARKER=1",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "docker.io/mcp/filesystem:latest",
                "/workspace",
                "--read-only",
            ]
        );
    }

    /// A `view = "primary"` sidecar joins the primary's namespaces: the nsfs
    /// *directory* and the launcher bind in with a plain `:ro` (never `,Z`,
    /// even under SELinux), `--userns=container:` replaces `keep-id`, the mount
    /// caps are added, and the launcher is the `--entrypoint`. The launcher
    /// argv (graft-prefixed program + bare target arg) rides the trailing slot,
    /// and `HOME` names the primary's home rather than the sidecar image's.
    #[test]
    fn podman_create_args_for_primary_view_join_the_primary() {
        let launch = ContainerLaunchSpec {
            primary_view: Some(PrimaryView {
                primary_container: "outrig-abc-primary".to_string(),
                primary_pid: 4242,
                helper_host: PathBuf::from("/sess/outrig-enter"),
                payload_home: "/home/tgockel".to_string(),
            }),
            ..Default::default()
        };
        let launcher_argv: Vec<String> = [
            "--ns-file",
            "/target-ns/mnt",
            "--graft",
            "/mnt",
            "--cwd",
            "/workspace",
            "--",
            "/mnt/usr/local/bin/node",
            "/mnt/app/dist/index.js",
            "/workspace",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // selinux=true to prove the nsfs/helper binds stay a plain `:ro`.
        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(
                ImageTag::new("docker.io/mcp/filesystem:latest"),
                launch,
                "outrig-abc-tools",
            )
            .with_args(launcher_argv),
            true,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-abc-tools",
                "-v",
                "/proc/4242/ns:/target-ns:ro",
                "-v",
                "/sess/outrig-enter:/outrig-enter:ro",
                "--userns=container:outrig-abc-primary",
                "--cap-add=SYS_ADMIN",
                "--cap-add=SYS_PTRACE",
                "--security-opt=no-new-privileges",
                "--entrypoint",
                "/outrig-enter",
                "--pull=never",
                "--env",
                "HOME=/home/tgockel",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "docker.io/mcp/filesystem:latest",
                "--ns-file",
                "/target-ns/mnt",
                "--graft",
                "/mnt",
                "--cwd",
                "/workspace",
                "--",
                "/mnt/usr/local/bin/node",
                "/mnt/app/dist/index.js",
                "/workspace",
            ]
        );
    }

    /// Every non-view container keeps the pre-change argv exactly: `keep-id`,
    /// no cap-adds, no nsfs bind, no `--entrypoint`.
    #[test]
    fn podman_create_args_without_view_are_byte_identical() {
        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(
                ImageTag::new("local:test"),
                ContainerLaunchSpec::default(),
                "outrig-test-noview",
            ),
            false,
            "org.outrig.attempt=testtoken",
        ));
        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-test-noview",
                "--userns=keep-id",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "local:test",
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
            labels: BTreeMap::new(),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--label",
                "org.outrig.attempt=testtoken",
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
            labels: BTreeMap::new(),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_render_devices_in_declaration_order() {
        let launch = ContainerLaunchSpec {
            devices: vec!["/dev/fuse".to_string(), "/dev/kvm".to_string()],
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--device=/dev/fuse",
                "--device=/dev/kvm",
                "--security-opt=no-new-privileges",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    /// Each entry gets its own `--security-opt`, and `ALL` reaches podman as
    /// written -- an image asking for `/proc/*` must not be widened into it.
    #[test]
    fn podman_run_args_render_unmask_entries_in_declaration_order() {
        let launch = ContainerLaunchSpec {
            unmask: vec!["/proc/*".to_string(), "ALL".to_string()],
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--security-opt=no-new-privileges",
                "--security-opt=unmask=/proc/*",
                "--security-opt=unmask=ALL",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    /// The measured recipe for a nested rootless podman, pinned whole so the
    /// combination cannot rot one flag at a time; `doc/concepts/containers.md`
    /// explains what each one buys. Note what is *not* here:
    /// `no_new_privileges` stays on.
    #[test]
    fn podman_run_args_carry_the_whole_nested_runtime_recipe() {
        let launch = ContainerLaunchSpec {
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::Default,
                cap_drop: Vec::new(),
                cap_add: vec!["SYS_ADMIN".to_string()],
            },
            devices: vec!["/dev/fuse".to_string(), "/dev/net/tun".to_string()],
            unmask: vec!["/proc/*".to_string()],
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--cap-add=SYS_ADMIN",
                "--device=/dev/fuse",
                "--device=/dev/net/tun",
                "--security-opt=no-new-privileges",
                "--security-opt=unmask=/proc/*",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    /// Clearing `no_new_privileges` must remove exactly one flag. `--pull=never`
    /// and `--userns=keep-id` are the neighbors most at risk from an edit to the
    /// hardening tail, so this pins them explicitly.
    #[test]
    fn podman_run_args_drop_only_no_new_privileges_when_cleared() {
        let launch = ContainerLaunchSpec {
            no_new_privileges: false,
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    #[test]
    fn podman_run_args_combine_devices_and_privileges_with_drop_all() {
        let launch = ContainerLaunchSpec {
            capabilities: ContainerCapabilities {
                profile: CapabilityProfile::DropAll,
                cap_drop: Vec::new(),
                cap_add: vec!["SYS_ADMIN".to_string()],
            },
            devices: vec!["/dev/fuse".to_string()],
            no_new_privileges: false,
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
            "outrig-test",
            &launch,
            false,
            "org.outrig.attempt=testtoken",
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
                "--cap-add=SYS_ADMIN",
                "--device=/dev/fuse",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    /// Sidecars go out through `podman create`, which shares
    /// `append_launch_flags`, so all three keys must reach them too. Clearing
    /// `no_new_privileges` here also pins that `unmask` stands on its own: it
    /// is emitted with no `--security-opt=no-new-privileges` ahead of it.
    #[test]
    fn podman_create_args_carry_devices_and_privileges() {
        let launch = ContainerLaunchSpec {
            devices: vec!["/dev/fuse".to_string()],
            no_new_privileges: false,
            unmask: vec!["/proc/*".to_string()],
            ..Default::default()
        };

        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(ImageTag::new("local:test"), launch, "outrig-test-fetch"),
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert_eq!(
            args,
            vec![
                "podman",
                "create",
                "--name",
                "outrig-test-fetch",
                "--userns=keep-id",
                "--device=/dev/fuse",
                "--security-opt=unmask=/proc/*",
                "--pull=never",
                "--label",
                "org.outrig.attempt=testtoken",
                "--interactive",
                "--rm",
                "local:test",
            ]
        );
    }

    /// Each `with_*` is the only path to its field, and a field that never
    /// reaches the command line is a silently-dropped knob -- the failure mode
    /// an options struct makes easy. Asserts against the same builder
    /// `create_initialized` calls.
    #[test]
    fn every_create_option_setter_reaches_the_podman_command() {
        let options = ContainerCreateOptions::new(
            ImageTag::new("docker.io/mcp/filesystem:latest"),
            ContainerLaunchSpec::default(),
            "outrig-test-fs",
        )
        .with_env(BTreeMap::from([(
            "TOKEN".to_string(),
            "secret".to_string(),
        )]))
        .with_intercept_dns(true)
        .with_args(vec!["/workspace".to_string()]);

        let args = argv(build_podman_create_cmd(
            &options,
            false,
            "org.outrig.attempt=testtoken",
        ));

        assert!(args.contains(&"outrig-test-fs".to_string()), "{args:?}");
        assert!(
            args.contains(&"docker.io/mcp/filesystem:latest".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"TOKEN=secret".to_string()), "{args:?}");
        assert!(
            args.contains(&crate::network::INTERCEPT_DNS_NAMESERVER.to_string()),
            "{args:?}"
        );
        assert_eq!(
            args.last().expect("argv is non-empty"),
            "/workspace",
            "entrypoint args come last: {args:?}"
        );
    }

    /// A post-bootstrap handle with fixed ids, so an exec argv is the same on
    /// every machine. `attach` rather than a struct literal: it is the public
    /// path to an un-owned handle, so `Drop` fires no `podman rm -f` for a
    /// container that was never created. Only the bootstrap-set fields are
    /// touched afterward, because nothing public sets them without podman.
    fn bootstrapped_container() -> Container {
        let mut container =
            Container::attach("outrig-test-exec", ImageTag::new("local:test"), None, None);
        container.user_name = Some("dev".to_string());
        container.group_name = Some("dev".to_string());
        container.uid = 1000;
        container.gid = 1000;
        container
    }

    #[test]
    fn podman_exec_args_without_workdir_are_byte_identical() {
        let args = argv(
            bootstrapped_container()
                .build_exec_argv(&["id".to_string(), "-un".to_string()], &ExecOptions::new()),
        );
        assert_eq!(
            args,
            vec![
                "podman",
                "exec",
                "-i",
                "--user=1000:1000",
                "--env",
                "HOME=/home/dev",
                "outrig-test-exec",
                "id",
                "-un",
            ]
        );
    }

    /// Same reasoning as `every_create_option_setter_reaches_the_podman_command`,
    /// plus it pins where `--workdir` sits: after the `--env` block and before
    /// the container name, so the no-workdir argv above stays a strict prefix
    /// of this one.
    #[test]
    fn every_exec_option_setter_reaches_the_podman_command() {
        let options = ExecOptions::new()
            .with_env(BTreeMap::from([
                ("ZONE".to_string(), "utc".to_string()),
                ("TOKEN".to_string(), "secret".to_string()),
            ]))
            .with_workdir("/workspace/sub");

        let args = argv(bootstrapped_container().build_exec_argv(&["pwd".to_string()], &options));

        assert_eq!(
            args,
            vec![
                "podman",
                "exec",
                "-i",
                "--user=1000:1000",
                "--env",
                "HOME=/home/dev",
                // BTreeMap order, not insertion order.
                "--env",
                "TOKEN=secret",
                "--env",
                "ZONE=utc",
                "--workdir",
                "/workspace/sub",
                "outrig-test-exec",
                "pwd",
            ]
        );
    }
}
