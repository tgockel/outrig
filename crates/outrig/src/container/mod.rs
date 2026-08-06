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
use std::process::{Output, Stdio};
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

/// Maximum `_`-suffix retries before bootstrap gives up.
const BOOTSTRAP_RETRIES: usize = 10;

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
        // Register before spawning so a SIGKILL between the spawn call and
        // its return can still be cleaned up by the panic hook.
        track(&name);

        let cmd = build_podman_run_cmd(image, &name, &launch, selinux_enforcing().await);

        if let Err(e) = process::run_capture_logged(cmd, "podman", transcript.as_ref()).await {
            untrack(&name);
            return Err(e);
        }

        let workspace = match &launch.workspace {
            Some(workspace) => (workspace.host.clone(), workspace.container.clone()),
            None => (PathBuf::new(), PathBuf::new()),
        };
        Ok(Self::handle(
            name,
            image.clone(),
            workspace,
            transcript,
            ContainerOwnership::Owned,
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
        // As in start_named: register before spawning so a SIGKILL between
        // the spawn call and its return can still be cleaned up.
        track(&options.name);

        let create = build_podman_create_cmd(&options, selinux_enforcing().await);
        let init = Cmd::new("podman").arg("init").arg(&options.name);
        for cmd in [create, init] {
            let logged = process::run_capture_logged(cmd, "podman", options.transcript.as_ref());
            if let Err(e) = logged.await {
                // An init failure leaves the created container behind.
                spawn_detached_rm(&options.name);
                untrack(&options.name);
                return Err(e);
            }
        }

        let workspace = match &options.launch.workspace {
            Some(workspace) => (workspace.host.clone(), workspace.container.clone()),
            None => (PathBuf::new(), PathBuf::new()),
        };
        Ok(Self::handle(
            options.name,
            options.image,
            workspace,
            options.transcript,
            ContainerOwnership::Owned,
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
            name.into(),
            image_tag,
            workspace,
            transcript,
            ContainerOwnership::Attached,
            false,
        )
    }

    /// Handle constructor shared by every path that materializes a
    /// [`Container`], so a new field is threaded through one place.
    fn handle(
        name: String,
        image_tag: ImageTag,
        (host_workspace, container_workspace): (PathBuf, PathBuf),
        transcript: Option<Transcript>,
        ownership: ContainerOwnership,
        dns_preconfigured: bool,
    ) -> Self {
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
        if self.ownership == ContainerOwnership::Attached {
            self.disposed = true;
            return Ok(());
        }

        let secs = grace.as_secs().to_string();
        // `--ignore`: an already-gone container counts as stopped -- an
        // entrypoint-stdio sidecar exits with its server and `--rm` removes
        // it before this orderly stop runs. Other stop failures propagate.
        process::run_capture_logged(
            Cmd::new("podman")
                .args(["stop", "--ignore", "-t"])
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
        spawn_detached_rm(&self.name);
        untrack(&self.name);
    }
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
    let cmd = Cmd::new("podman")
        .args(["run", "-d", "--rm", "--name"])
        .arg(name);
    append_launch_flags(cmd, launch, selinux)
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
fn build_podman_create_cmd(options: &ContainerCreateOptions, selinux: bool) -> Cmd {
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

    cmd.args(["--interactive", "--rm"])
        .arg(options.image.as_str())
        .args(&options.args)
}

/// Flags shared by `podman run` and `podman create`: labels, workspace and
/// extra bind mounts, keep-id, workspace workdir, capability policy, device
/// passthrough, and the hardening tail. `--security-opt=no-new-privileges` is
/// part of that tail only when the launch spec keeps it.
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
            labels: BTreeMap::new(),
            ..Default::default()
        };

        let args = argv(build_podman_run_cmd(
            &ImageTag::new("local:test"),
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
                "local:test",
                "sleep",
                "infinity",
            ]
        );
    }

    /// Sidecars go out through `podman create`, which shares
    /// `append_launch_flags`, so both keys must reach them too.
    #[test]
    fn podman_create_args_carry_devices_and_privileges() {
        let launch = ContainerLaunchSpec {
            devices: vec!["/dev/fuse".to_string()],
            no_new_privileges: false,
            ..Default::default()
        };

        let args = argv(build_podman_create_cmd(
            &ContainerCreateOptions::new(ImageTag::new("local:test"), launch, "outrig-test-fetch"),
            false,
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
                "--pull=never",
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

        let args = argv(build_podman_create_cmd(&options, false));

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
