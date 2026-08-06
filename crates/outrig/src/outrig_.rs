//! Curated facade for library callers. Composes existing primitives
//! (`image::ensure_image`, `Container`, `McpClient`) into a single
//! `launch -> tools/call_tool -> shutdown` flow that doesn't expose
//! internals like rig, the REPL, or session storage.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Child;

use crate::config::{
    CapabilityProfile, Config, ContainerSecurity, EnvValue, ImageConfig, ImageSourceRef,
    McpServerSpec, MountAccess, NetworkMode, NetworkPolicy, SidecarStart, SidecarView,
    SidecarWorkspaceAccess, check_entrypoint_hosting, check_sidecar_image, check_sidecar_name,
    check_view_exclusions, is_valid_mcp_server_name,
};
use crate::container::{
    Container, ContainerCapabilities, ContainerCreateOptions, ContainerLaunchSpec, ContainerMount,
    ContainerWorkspace, ExecOptions, LABEL_SESSION, LABEL_SIDECAR, PrimaryView,
    embedded::{self, McpDeclarationSource},
    enter,
    sidecar::{self, Placement, SessionMcpPlan},
};
use crate::error::{IoPathExt, OutrigError, Result};
use crate::image::{self, ImageTag};
use crate::mcp::{McpClient, McpToolResult};
use crate::network::NetworkInterceptor;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Where the container's image comes from. Either a Dockerfile + context
/// (built via buildah, with the project's content-addressed cache) or an
/// already-built image tag (used verbatim).
pub(crate) enum LaunchSource {
    Build {
        dockerfile: PathBuf,
        context: PathBuf,
        build_args: BTreeMap<String, EnvValue>,
    },
    Image {
        tag: String,
    },
}

/// Host directory mounted into the container as the workspace, plus the
/// in-container path it gets mounted at.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WorkspaceSpec {
    pub host: PathBuf,
    pub container: PathBuf,
}

impl WorkspaceSpec {
    /// Mount `host` as the workspace, visible at `container` inside.
    pub fn new(host: impl Into<PathBuf>, container: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            container: container.into(),
        }
    }
}

/// Extra host directory mounted into the container.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MountSpec {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

impl MountSpec {
    /// Mount `host` at `container` with the given access.
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

/// Container security policy applied at launch.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SecuritySpec {
    pub capabilities: CapabilitySpec,
    /// Host device nodes to pass through, one `--device=<path>` each.
    pub devices: Vec<String>,
    /// Whether to apply `--security-opt=no-new-privileges`. Clearing this
    /// restores setuid escalation inside the container, which is what a nested
    /// rootless container runtime needs; see `doc/concepts/containers.md`.
    pub no_new_privileges: bool,
}

/// Hand-written rather than derived so that `no_new_privileges` defaults to
/// `true`. Devices and privilege escalation are deliberately siblings of
/// `capabilities` rather than fields inside [`CapabilitySpec`]: a device node
/// is not a capability, and `no_new_privs` is a separate process flag.
impl Default for SecuritySpec {
    fn default() -> Self {
        Self {
            capabilities: CapabilitySpec::default(),
            devices: Vec::new(),
            no_new_privileges: true,
        }
    }
}

/// Linux capability profile plus explicit capability overrides.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CapabilitySpec {
    pub profile: CapabilityProfile,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
}

impl CapabilitySpec {
    /// `profile` with no explicit per-capability overrides. Assign `cap_drop`
    /// / `cap_add` on the result to add them.
    pub fn new(profile: CapabilityProfile) -> Self {
        Self {
            profile,
            ..Self::default()
        }
    }
}

/// Network monitoring policy applied at launch.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NetworkSpec {
    pub mode: NetworkMode,
    pub policy: Option<NetworkPolicy>,
}

/// One MCP server hosted by a [`SidecarSpec`] sidecar, in either of the two
/// transports a sidecar supports.
///
/// The distinction is the same one the config path draws through
/// [`McpServerSpec::is_entrypoint_stdio`]: an exec-stdio server is one process
/// among many in a container that outlives it, while an entrypoint-stdio
/// server *is* the container process. Modeling it as two variants rather than
/// an optional command keeps "no command and no args" unrepresentable.
///
/// Deliberately not [`McpServerSpec`], which it otherwise resembles: that type
/// also carries the placement keys (`sidecar`, `image`, `view`) that decide
/// *which* container hosts a server, and here the enclosing [`SidecarSpec`]
/// has already answered that. A caller building a sidecar cannot express a
/// contradictory placement because there is nowhere to write one.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SidecarServerSpec {
    /// Spawned with `podman exec -i` inside a running sidecar. Any number of
    /// these can share one container.
    #[non_exhaustive]
    ExecStdio {
        command: Vec<String>,
        env: BTreeMap<String, EnvValue>,
    },
    /// The container's own `ENTRYPOINT`, spoken to over `podman start
    /// --attach --interactive`. `args` are its positional arguments, which is
    /// how off-the-shelf MCP images take the directories they serve. A
    /// sidecar hosting one of these hosts nothing else: container lifetime is
    /// server lifetime.
    #[non_exhaustive]
    Entrypoint {
        args: Vec<String>,
        env: BTreeMap<String, EnvValue>,
    },
}

impl SidecarServerSpec {
    /// A server exec'd as `command`, with no extra environment. Mirrors
    /// [`McpServerSpec::exec`].
    pub fn exec(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::ExecStdio {
            command: command.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
        }
    }

    /// A server that is its container's `ENTRYPOINT`, handed `args`. Mirrors
    /// [`McpServerSpec::entrypoint`] -- the image comes from the enclosing
    /// [`SidecarSpec`], so it is not repeated here.
    pub fn entrypoint(args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Entrypoint {
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
        }
    }

    /// Environment for the server process, resolved at spawn time. Sealed
    /// variants cannot be updated field-wise from outside, so this is the
    /// only way to attach one.
    pub fn with_env(mut self, env: BTreeMap<String, EnvValue>) -> Self {
        match &mut self {
            Self::ExecStdio { env: slot, .. } | Self::Entrypoint { env: slot, .. } => *slot = env,
        }
        self
    }

    /// The argv to exec, or `None` for the entrypoint form.
    pub fn command(&self) -> Option<&[String]> {
        match self {
            Self::ExecStdio { command, .. } => Some(command),
            Self::Entrypoint { .. } => None,
        }
    }

    /// Positional arguments for the entrypoint form. Always empty for
    /// exec-stdio, which carries a full argv already.
    pub fn args(&self) -> &[String] {
        match self {
            Self::ExecStdio { .. } => &[],
            Self::Entrypoint { args, .. } => args,
        }
    }

    /// The declared (still-unresolved) environment.
    pub fn env(&self) -> &BTreeMap<String, EnvValue> {
        match self {
            Self::ExecStdio { env, .. } | Self::Entrypoint { env, .. } => env,
        }
    }

    /// Whether this server is its container's `ENTRYPOINT`.
    pub fn is_entrypoint(&self) -> bool {
        matches!(self, Self::Entrypoint { .. })
    }
}

/// Description of one sidecar container: image (a raw podman ref, used
/// verbatim like [`LaunchSpec::from_image`]), workspace visibility, filesystem
/// view, extra mounts, security policy, and the MCP servers it hosts. Passed
/// to [`Outrig::add_sidecar`] mid-session or declared at launch via
/// [`LaunchSpec::with_sidecar`].
///
/// The fields mirror `[sidecars.<sc>]`, minus `start` and `on-failure`: a
/// library caller starts a sidecar by calling [`Outrig::add_sidecar`], and
/// [`LaunchSpec::with_sidecar`] is abort-only by design.
///
/// Config-name image resolution (the `[images.<name>]` lookup the CLI
/// performs) is out of facade scope: callers who want a built image run
/// `image::ensure_image` themselves and pass the resulting tag.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SidecarSpec {
    pub name: String,
    pub(crate) image: String,
    pub workspace: SidecarWorkspaceAccess,
    /// Whether the sidecar runs against the primary container's filesystem
    /// view. [`SidecarView::Primary`] is entrypoint-stdio only, mutually
    /// exclusive with `workspace`, and needs the `outrig-enter` helper.
    pub view: SidecarView,
    pub mounts: Vec<MountSpec>,
    pub security: SecuritySpec,
    pub servers: BTreeMap<String, SidecarServerSpec>,
}

impl SidecarSpec {
    /// A sidecar named `name` running `image` (a podman ref used verbatim,
    /// no build or pull): no workspace view, its own filesystem view, no
    /// mounts, default security, no servers.
    pub fn from_image(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            workspace: SidecarWorkspaceAccess::None,
            view: SidecarView::None,
            mounts: Vec::new(),
            security: SecuritySpec::default(),
            servers: BTreeMap::new(),
        }
    }

    /// How much of the session workspace the sidecar sees (default: none).
    pub fn with_workspace_access(mut self, access: SidecarWorkspaceAccess) -> Self {
        self.workspace = access;
        self
    }

    /// Run the sidecar's server against the primary container's filesystem
    /// view (default: its own image's). [`SidecarView::Primary`] is a real
    /// posture change -- see [`Outrig::add_sidecar`].
    pub fn with_view(mut self, view: SidecarView) -> Self {
        self.view = view;
        self
    }

    pub fn with_mount(mut self, mount: MountSpec) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn with_mounts(mut self, mounts: impl IntoIterator<Item = MountSpec>) -> Self {
        self.mounts.extend(mounts);
        self
    }

    pub fn with_security(mut self, security: SecuritySpec) -> Self {
        self.security = security;
        self
    }

    /// Host an exec-stdio MCP server named `name` running `command`.
    pub fn with_server(self, name: impl Into<String>, command: Vec<String>) -> Self {
        self.with_server_env(name, command, BTreeMap::new())
    }

    /// [`SidecarSpec::with_server`] plus per-server environment variables.
    pub fn with_server_env(
        self,
        name: impl Into<String>,
        command: Vec<String>,
        env: BTreeMap<String, EnvValue>,
    ) -> Self {
        self.with_server_spec(name, SidecarServerSpec::exec(command).with_env(env))
    }

    /// Host this container's `ENTRYPOINT` as an MCP server named `name`,
    /// handing it `args`. No command is written anywhere: the image supplies
    /// it. Such a sidecar hosts exactly this one server.
    pub fn with_entrypoint_server(
        self,
        name: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.with_server_spec(name, SidecarServerSpec::entrypoint(args))
    }

    /// Host `server` under `name`. The general form the other `with_*server`
    /// methods are shorthands for -- reach for it when a server needs
    /// environment as well as the entrypoint form:
    /// `with_server_spec("fs", SidecarServerSpec::entrypoint(args).with_env(env))`.
    pub fn with_server_spec(mut self, name: impl Into<String>, server: SidecarServerSpec) -> Self {
        self.servers.insert(name.into(), server);
        self
    }
}

/// How [`Outrig::launch`] handles MCP servers declared in an image's
/// `org.outrig.mcp` label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum EmbeddedMcpPolicy {
    /// Merge image-embedded declarations with the launch spec's MCP map.
    /// Launch-spec entries replace image entries with the same server name.
    #[default]
    Merge,
    /// Ignore image-embedded declarations and use only the launch spec's MCP map.
    Ignore,
}

impl From<&ContainerSecurity> for SecuritySpec {
    fn from(security: &ContainerSecurity) -> Self {
        Self {
            capabilities: CapabilitySpec {
                profile: security.capability_profile,
                cap_drop: security.cap_drop.clone(),
                cap_add: security.cap_add.clone(),
            },
            devices: security.devices.clone(),
            no_new_privileges: security.no_new_privileges,
        }
    }
}

impl From<&CapabilitySpec> for ContainerCapabilities {
    fn from(capabilities: &CapabilitySpec) -> Self {
        Self {
            profile: capabilities.profile,
            cap_drop: capabilities.cap_drop.clone(),
            cap_add: capabilities.cap_add.clone(),
        }
    }
}

/// The capability third of a `[security]` block, in the shape `podman` wants.
/// Lives here rather than at each call site because both types are
/// `#[non_exhaustive]`: a new capability knob can only be wired through inside
/// this crate, so a caller's own copy of this mapping would keep compiling
/// while silently dropping it.
impl From<&ContainerSecurity> for ContainerCapabilities {
    fn from(security: &ContainerSecurity) -> Self {
        Self {
            profile: security.capability_profile,
            cap_drop: security.cap_drop.clone(),
            cap_add: security.cap_add.clone(),
        }
    }
}

/// Description of one container launch: image source, optional workspace
/// mount, MCP servers to start inside, and the directory to land per-server
/// stderr in.
#[non_exhaustive]
pub struct LaunchSpec {
    pub(crate) source: LaunchSource,
    pub workspace: Option<WorkspaceSpec>,
    pub mounts: Vec<MountSpec>,
    pub security: SecuritySpec,
    pub network: NetworkSpec,
    pub embedded_mcp_policy: EmbeddedMcpPolicy,
    pub mcp: BTreeMap<String, McpServerSpec>,
    pub sidecars: Vec<SidecarSpec>,
    pub log_dir: PathBuf,
}

impl LaunchSpec {
    /// Build the container's image from a Dockerfile, then launch it.
    /// Caller-supplied `dockerfile` and `context` should be absolute (or
    /// relative to the current directory) -- they are forwarded as-is to
    /// the buildah invocation.
    pub fn build(
        dockerfile: PathBuf,
        context: PathBuf,
        build_args: BTreeMap<String, String>,
        workspace: WorkspaceSpec,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self {
        let build_args = build_args
            .into_iter()
            .map(|(key, value)| (key, EnvValue::Literal(value)))
            .collect();
        Self {
            source: LaunchSource::Build {
                dockerfile,
                context,
                build_args,
            },
            workspace: Some(workspace),
            mounts: Vec::new(),
            security: SecuritySpec::default(),
            network: NetworkSpec::default(),
            embedded_mcp_policy: EmbeddedMcpPolicy::default(),
            mcp,
            sidecars: Vec::new(),
            log_dir,
        }
    }

    /// Use an already-built image tag verbatim. No buildah invocation, no
    /// cache lookup -- the tag is passed straight to `podman run`.
    pub fn from_image(
        image: impl Into<String>,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            source: LaunchSource::Image { tag: image.into() },
            workspace: None,
            mounts: Vec::new(),
            security: SecuritySpec::default(),
            network: NetworkSpec::default(),
            embedded_mcp_policy: EmbeddedMcpPolicy::default(),
            mcp,
            sidecars: Vec::new(),
            log_dir,
        }
    }

    /// Build a `LaunchSpec` from a parsed [`Config`], selecting the primary
    /// image by name. `image_name` must name an `[images.<name>]` block --
    /// raw primary refs are [`LaunchSpec::from_image`]'s job. Repo-relative
    /// paths resolve against `repo_root` so the spec carries absolute paths.
    ///
    /// Unlike a verbatim copy of the image's `[mcp]` map, this resolves MCP
    /// placement into sidecars: top-level `[sidecars.<sc>]` blocks and every
    /// placement-bearing entry -- `sidecar = "<sc>"`, inline `image = "..."`,
    /// entrypoint-stdio, and `view = "primary"` alike -- become [`SidecarSpec`]s
    /// on the returned spec. Named-sidecar and anonymous images resolve like
    /// `--image` (a sibling `[images.<name>]` block first, else a raw local ref)
    /// and are **built/pulled eagerly here**; the primary image is still built
    /// lazily by `launch`.
    ///
    /// Scope of the library translation (the CLI path is a strict superset):
    /// - `start = "manual"` sidecars are skipped -- neither started nor carried.
    ///   Rebuild a [`SidecarSpec`] and call [`Outrig::add_sidecar`] to start one
    ///   mid-session.
    /// - `on-failure = "warn"` is not honored: launch-time sidecars are
    ///   abort-only here, per [`LaunchSpec::with_sidecar`].
    /// - A sidecar image's own `org.outrig.mcp` label is not merged; sidecar
    ///   servers come only from the repo config's placement entries.
    ///
    /// Assumes `config` has passed validation (see [`Config`]); an unvalidated
    /// config can silently drop a server that names a missing sidecar block.
    pub async fn from_config(
        config: &Config,
        image_name: &str,
        repo_root: &Path,
        log_dir: PathBuf,
    ) -> Result<Self> {
        let cfg = config.images.get(image_name).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "image-config {image_name:?} does not match any [images.<name>]"
            ))
        })?;

        let ws = WorkspaceSpec::new(
            config.workspace.resolved_host_path(repo_root),
            config.workspace.container_path.clone(),
        );
        let mounts = config
            .workspace
            .mounts
            .iter()
            .map(|mount| {
                MountSpec::new(
                    mount.resolved_host_path(repo_root),
                    mount.container_path.clone(),
                    mount.access,
                )
            })
            .collect();
        let source = match cfg.source() {
            ImageSourceRef::Build { build_args, .. } => {
                let (dockerfile, context) = cfg.resolved_build_paths(repo_root);
                LaunchSource::Build {
                    dockerfile,
                    context,
                    build_args: build_args.clone(),
                }
            }
            ImageSourceRef::Image { image_name } => LaunchSource::Image {
                tag: image_name.to_string(),
            },
        };

        let plan = sidecar::plan_from_config(config, cfg);
        let (mcp, mut sidecars) = plan_to_launch_parts(&plan, repo_root);
        for sidecar in &mut sidecars {
            sidecar.image = resolve_sidecar_image_tag(config, repo_root, &sidecar.image).await?;
        }

        Ok(Self {
            source,
            workspace: Some(ws),
            mounts,
            security: SecuritySpec::from(&cfg.security),
            network: NetworkSpec::default(),
            embedded_mcp_policy: EmbeddedMcpPolicy::default(),
            mcp,
            sidecars,
            log_dir,
        })
    }

    pub fn with_workspace(mut self, workspace: WorkspaceSpec) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn without_workspace(mut self) -> Self {
        self.workspace = None;
        self
    }

    pub fn with_mount(mut self, mount: MountSpec) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn with_mounts(mut self, mounts: impl IntoIterator<Item = MountSpec>) -> Self {
        self.mounts.extend(mounts);
        self
    }

    pub fn with_security(mut self, security: SecuritySpec) -> Self {
        self.security = security;
        self
    }

    pub fn with_capabilities(mut self, capabilities: CapabilitySpec) -> Self {
        self.security.capabilities = capabilities;
        self
    }

    pub fn with_network_mode(mut self, mode: NetworkMode) -> Self {
        self.network.mode = mode;
        self
    }

    pub fn with_network_filter(mut self, policy: NetworkPolicy) -> Self {
        self.network.mode = NetworkMode::Filter;
        self.network.policy = Some(policy);
        self
    }

    pub fn with_embedded_mcp_policy(mut self, policy: EmbeddedMcpPolicy) -> Self {
        self.embedded_mcp_policy = policy;
        self
    }

    /// Declare a sidecar to start with the session. Launch-time sidecars are
    /// abort-only: any sidecar failure fails the launch and tears down
    /// everything already started (a library caller holds the `Result` and
    /// can relaunch without the sidecar; the config-level `on-failure =
    /// "warn"` convenience is a CLI concern).
    pub fn with_sidecar(mut self, sidecar: SidecarSpec) -> Self {
        self.sidecars.push(sidecar);
        self
    }
}

/// Split a planned session into the primary MCP map and the launch-time
/// sidecars. Pure -- no image resolution, so each `SidecarSpec.image` still
/// holds the unresolved config ref for [`LaunchSpec::from_config`] to rewrite.
/// `start = "manual"` sidecars are dropped (their servers, being sidecar-placed,
/// are already absent from the primary map); every other placement lowers,
/// entrypoint-stdio included.
fn plan_to_launch_parts(
    plan: &SessionMcpPlan,
    repo_root: &Path,
) -> (BTreeMap<String, McpServerSpec>, Vec<SidecarSpec>) {
    let mcp = plan
        .servers
        .iter()
        .filter(|(_, placed)| placed.placement == Placement::Primary)
        .map(|(name, placed)| (name.clone(), placed.spec.clone()))
        .collect();

    let mut sidecars = Vec::new();
    for sc in plan.sidecars.values() {
        if sc.start != SidecarStart::Auto {
            continue;
        }
        // An entrypoint host serves exactly one server, and its arguments come
        // from the entry or the block -- `entrypoint_args` is the one place
        // that picks between them, so the spec needs only the single slot.
        let servers = match plan.entrypoint_server_in(sc) {
            Some((name, placed)) => BTreeMap::from([(
                name.clone(),
                SidecarServerSpec::entrypoint(sidecar::entrypoint_args(&placed.spec, sc).to_vec())
                    .with_env(placed.spec.env().clone()),
            )]),
            None => plan
                .servers_in(&sc.name)
                .map(|(name, placed)| {
                    let (command, env) = placed.spec.normalize();
                    (name.clone(), SidecarServerSpec::exec(command).with_env(env))
                })
                .collect(),
        };
        let mounts = sc
            .mounts
            .iter()
            .map(|mount| {
                MountSpec::new(
                    mount.resolved_host_path(repo_root),
                    mount.container_path.clone(),
                    mount.access,
                )
            })
            .collect();
        sidecars.push(SidecarSpec {
            name: sc.name.clone(),
            image: sc.image.clone(),
            workspace: sc.workspace,
            view: sc.view,
            mounts,
            security: SecuritySpec::from(&sc.security),
            servers,
        });
    }
    (mcp, sidecars)
}

/// Resolve a sidecar image ref like `--image`: a sibling `[images.<name>]`
/// config builds/pulls through the content-hash cache; an unmatched name is a
/// raw podman ref that must already be present locally (no pull). Returns the
/// resolved tag used verbatim as [`SidecarSpec::image`]. Mirrors the CLI's
/// `ensure_sidecar_image`.
async fn resolve_sidecar_image_tag(
    config: &Config,
    repo_root: &Path,
    image_ref: &str,
) -> Result<String> {
    match config.images.get(image_ref) {
        Some(image_cfg) => {
            let tag = image::compute_tag_for(image_ref, image_cfg, repo_root).await?;
            image::ensure_tagged_image_for(image_ref, image_cfg, repo_root, &tag, false, None)
                .await?;
            Ok(tag.into_string())
        }
        None => {
            let tag = ImageTag::new(image_ref);
            image::ensure_local_image(&tag, None).await?;
            Ok(tag.into_string())
        }
    }
}

/// Description of a single tool exposed by one of the running MCP servers.
/// `server` is the local config name (`spec.mcp` key) and `name` is the
/// tool name as advertised by the server (un-namespaced).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolHandle {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// A running container set (one primary plus any sidecars) with MCP servers
/// attached. Construct via [`Outrig::launch`]; grow via
/// [`Outrig::add_sidecar`]; clean up via [`Outrig::shutdown`]. Dropping
/// without `shutdown` still removes every container via a detached
/// `podman rm -f` (plus the panic-hook sweeper if the host installed it).
pub struct Outrig {
    // Declared before `container` so field-order `Drop` reaps sidecars
    // before the primary, mirroring orderly shutdown.
    sidecars: BTreeMap<String, Container>,
    container: Container,
    clients: BTreeMap<String, Arc<McpClient>>,
    tools: Vec<ToolHandle>,
    network: Option<NetworkInterceptor>,
    log_dir: PathBuf,
}

impl Outrig {
    /// Acquire the image, start the container, bootstrap the runtime user,
    /// resolve MCP config according to `spec.embedded_mcp_policy`, connect every
    /// resolved MCP server, and index their tools.
    /// Returns once every server has answered an initial `tools/list`.
    pub async fn launch(spec: &LaunchSpec) -> Result<Self> {
        let image_tag = match &spec.source {
            LaunchSource::Build {
                dockerfile,
                context,
                build_args,
            } => {
                // Reuse `ensure_image` by wrapping the raw inputs in a
                // `ImageConfig`. The spec's paths are already absolute and the
                // config records no `ConfigSource`, so the empty `repo_root`
                // fallback makes the resolution a no-op.
                let mut cfg = ImageConfig::from_dockerfile(dockerfile.clone(), context.clone());
                cfg.build_args = build_args.clone();
                image::ensure_image(&cfg, Path::new(""), false).await?.tag
            }
            LaunchSource::Image { tag } => ImageTag::new(tag.clone()),
        };

        let launch = ContainerLaunchSpec {
            workspace: spec.workspace.as_ref().map(|workspace| {
                ContainerWorkspace::new(
                    workspace.host.clone(),
                    workspace.container.clone(),
                    MountAccess::ReadWrite,
                )
            }),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| {
                    ContainerMount::new(mount.host.clone(), mount.container.clone(), mount.access)
                })
                .collect(),
            capabilities: ContainerCapabilities::from(&spec.security.capabilities),
            devices: spec.security.devices.clone(),
            no_new_privileges: spec.security.no_new_privileges,
            labels: BTreeMap::new(),
            // The programmatic path never hosts entrypoint-stdio servers, so
            // it never uses the primary-view placement.
            primary_view: None,
        };
        let mut container = Container::start(&image_tag, launch).await?;
        container.bootstrap_user().await?;

        let network = match spec.network.mode {
            NetworkMode::Default => None,
            NetworkMode::Audit => Some(
                NetworkInterceptor::start(&container, &spec.log_dir, container.session_suffix())
                    .await?,
            ),
            NetworkMode::Filter => {
                let policy = spec.network.policy.clone().ok_or_else(|| {
                    crate::error::OutrigError::Configuration(
                        "network filter mode requires a NetworkPolicy; use \
                         LaunchSpec::with_network_filter(policy)"
                            .to_string(),
                    )
                })?;
                policy
                    .validate(true)
                    .map_err(crate::error::OutrigError::Configuration)?;
                Some(
                    NetworkInterceptor::start_with_policy(
                        &container,
                        &spec.log_dir,
                        container.session_suffix(),
                        policy,
                    )
                    .await?,
                )
            }
        };

        let mcp = match spec.embedded_mcp_policy {
            EmbeddedMcpPolicy::Merge => {
                embedded::merged_mcp_with_source(
                    &container,
                    &spec.mcp,
                    McpDeclarationSource::LaunchSpec,
                )
                .await?
            }
            EmbeddedMcpPolicy::Ignore => {
                embedded::mcp_with_source(&spec.mcp, McpDeclarationSource::LaunchSpec)
            }
        };

        // The effective MCP map runs in the primary; a placement-bearing
        // entry would silently run there otherwise. `LaunchSpec::from_config`
        // translates config placement into `SidecarSpec`s, so this guards
        // hand-built specs -- the library route for sidecar-hosted servers is
        // a `SidecarSpec`, which carries its own servers.
        for (name, server) in &mcp {
            if server.spec.sidecar().is_some() || server.spec.image().is_some() {
                return Err(OutrigError::Configuration(format!(
                    "mcp server {name:?} declares a sidecar placement; the library API \
                     hosts sidecar servers via LaunchSpec::with_sidecar / \
                     Outrig::add_sidecar -- declare a SidecarSpec instead"
                )));
            }
        }

        let mut clients: BTreeMap<String, Arc<McpClient>> = BTreeMap::new();
        let mut tools: Vec<ToolHandle> = Vec::new();
        for (name, server) in &mcp {
            let client = McpClient::connect_via_podman_exec_with_source(
                &container,
                &server.spec,
                name,
                server.source,
                &spec.log_dir,
                &BTreeMap::new(),
            )
            .await?;
            tools.extend(tool_handles(name, client.list_tools().await?));
            clients.insert(name.clone(), Arc::new(client));
        }

        let mut outrig = Self {
            sidecars: BTreeMap::new(),
            container,
            clients,
            tools,
            network,
            log_dir: spec.log_dir.clone(),
        };
        for sidecar in &spec.sidecars {
            if let Err(e) = outrig.add_sidecar(sidecar.clone()).await {
                let _ = outrig.shutdown().await;
                return Err(e);
            }
        }
        Ok(outrig)
    }

    /// Start a sidecar container mid-session: labels, keep-id, conditional
    /// user bootstrap, network-interceptor attachment (when the session
    /// launched with one), and one MCP connection per server in the spec. On
    /// success the new servers' tools are appended to [`Outrig::tools`] and
    /// returned. On failure everything started by this call is torn down
    /// (clients, interceptor attachment, container) and the session is left
    /// exactly as it was -- errors reach only the caller.
    ///
    /// Both sidecar transports are supported. An **exec-stdio** sidecar is
    /// started and its servers are `podman exec`'d into it, so the container
    /// outlives any one server. An **entrypoint-stdio** sidecar (a spec built
    /// with [`SidecarSpec::with_entrypoint_server`]) is created with its
    /// server's environment baked in and only starts when the server does:
    /// container lifetime *is* server lifetime, so the server exiting removes
    /// the container, surfacing like any mid-session sidecar death.
    ///
    /// With [`SidecarView::Primary`] the sidecar additionally joins the
    /// primary's mount namespace through the `outrig-enter` helper, which is
    /// materialized into the session's log directory. That is a real posture
    /// change -- the sidecar runs with `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` in
    /// the primary's user namespace and can read the primary's whole
    /// filesystem. A build without the helper fails here rather than emitting
    /// a broken container.
    pub async fn add_sidecar(&mut self, spec: SidecarSpec) -> Result<Vec<ToolHandle>> {
        validate_sidecar_spec(&spec, &self.clients, &self.sidecars, enter::is_available())?;

        let workspace_access = spec.workspace.mount_access();
        if workspace_access.is_some() && self.container.host_workspace().as_os_str().is_empty() {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?} requests workspace access but the session has no workspace",
                spec.name
            )));
        }
        let launch = ContainerLaunchSpec {
            workspace: workspace_access.map(|access| {
                ContainerWorkspace::new(
                    self.container.host_workspace(),
                    self.container.container_workspace(),
                    access,
                )
            }),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| {
                    ContainerMount::new(mount.host.clone(), mount.container.clone(), mount.access)
                })
                .collect(),
            capabilities: ContainerCapabilities::from(&spec.security.capabilities),
            devices: spec.security.devices.clone(),
            no_new_privileges: spec.security.no_new_privileges,
            labels: BTreeMap::from([
                (
                    LABEL_SESSION.to_string(),
                    self.container.session_suffix().to_string(),
                ),
                (LABEL_SIDECAR.to_string(), spec.name.clone()),
            ]),
            // Set by `create_entrypoint_sidecar` for a `view = "primary"`
            // sidecar; every other placement leaves it `None`.
            primary_view: None,
        };
        let container_name =
            crate::container::sidecar_container_name(self.container.session_suffix(), &spec.name);
        let image = ImageTag::new(spec.image.clone());

        // Validation guarantees an entrypoint host serves exactly one server,
        // so the first is the only one.
        let entrypoint_server = spec
            .servers
            .iter()
            .find(|(_, server)| server.is_entrypoint());

        let mut container = match entrypoint_server {
            Some((name, server)) => {
                self.create_entrypoint_sidecar(&spec, name, server, launch, image, container_name)
                    .await?
            }
            None => Container::start_named(&image, launch, container_name, None).await?,
        };

        // Bootstrap only where identity matters; `bootstrap_needed` owns the
        // entrypoint-host exemption, so both paths get it from one place.
        let needs_bootstrap = sidecar::bootstrap_needed(
            entrypoint_server.is_some(),
            !spec.servers.is_empty(),
            spec.workspace,
            !spec.mounts.is_empty(),
        );
        if needs_bootstrap && let Err(e) = container.bootstrap_user().await {
            let _ = container.stop(SHUTDOWN_GRACE).await;
            return Err(e);
        }

        if let Some(network) = &mut self.network
            && let Err(e) = network.attach(&container).await
        {
            let _ = container.stop(SHUTDOWN_GRACE).await;
            return Err(e);
        }

        match connect_sidecar_servers(&container, &spec, &self.log_dir).await {
            Ok((clients, tools)) => {
                self.sidecars.insert(spec.name.clone(), container);
                self.clients
                    .extend(clients.into_iter().map(|(name, c)| (name, Arc::new(c))));
                self.tools.extend(tools.iter().cloned());
                Ok(tools)
            }
            Err(e) => {
                if let Some(network) = &mut self.network {
                    let _ = network.detach(container.name()).await;
                }
                let _ = container.stop(SHUTDOWN_GRACE).await;
                Err(e)
            }
        }
    }

    /// `podman create` + `podman init` an entrypoint-stdio sidecar without
    /// running its ENTRYPOINT: the interceptor attaches to the initialized
    /// network namespace first, and the `podman start --attach` that finally
    /// runs the server happens later, in [`connect_sidecar_servers`]. The
    /// server's environment is baked in here because `podman start` carries no
    /// `--env`.
    ///
    /// A `view = "primary"` sidecar additionally resolves the primary's
    /// namespaces and materializes the `outrig-enter` launcher *before*
    /// anything is created, so a missing helper or a not-yet-running primary
    /// fails without leaving a container behind.
    async fn create_entrypoint_sidecar(
        &self,
        spec: &SidecarSpec,
        server_name: &str,
        server: &SidecarServerSpec,
        mut launch: ContainerLaunchSpec,
        image: ImageTag,
        container_name: String,
    ) -> Result<Container> {
        let (image_entrypoint, image_cmd) = if spec.view == SidecarView::Primary {
            // The log dir is otherwise created lazily by the first MCP
            // connection, which for an entrypoint server happens after the
            // helper is already needed.
            tokio::fs::create_dir_all(&self.log_dir)
                .await
                .path_ctx("create directory", &self.log_dir)?;
            // Two unrelated podman inspects -- the primary's PID and the
            // sidecar image's config -- so they overlap.
            let (pid, payload) = tokio::try_join!(
                self.container.pid(),
                image::read_image_entrypoint_cmd(&image, None),
            )?;
            // `home_dir` is `Some` here: the session's primary is bootstrapped
            // at start, and `add_sidecar` runs against a started session.
            let payload_home = self.container.home_dir().ok_or_else(|| {
                OutrigError::Configuration(
                    "a view = \"primary\" sidecar needs the primary's user, which has not been \
                     bootstrapped"
                        .to_string(),
                )
            })?;
            launch.primary_view = Some(PrimaryView::new(
                self.container.name(),
                pid,
                enter::materialize(&self.log_dir)?,
                payload_home,
            ));
            payload
        } else {
            (Vec::new(), Vec::new())
        };

        let options = ContainerCreateOptions::new(image, launch, container_name)
            .with_env(crate::mcp::resolve_mcp_env(
                server_name,
                server.env().clone(),
                &BTreeMap::new(),
            )?)
            .with_intercept_dns(self.network.is_some())
            .with_args(sidecar::entrypoint_create_args(
                spec.view,
                &image_entrypoint,
                &image_cmd,
                server.args(),
                self.container.container_workspace(),
                Some((self.container.uid(), self.container.gid())),
            ));
        Container::create_initialized(options).await
    }

    /// Tools advertised by every connected MCP server: launch-time servers in
    /// `(server, name)` order matching the effective MCP map's `BTreeMap`
    /// iteration followed by each server's advertised order, then each
    /// [`Outrig::add_sidecar`]'s tools in call order.
    pub fn tools(&self) -> &[ToolHandle] {
        &self.tools
    }

    /// Run `argv` in the **primary** container as the session's runtime user,
    /// with all three stdio streams piped back to the caller. See
    /// [`ExecOptions`] for the environment and working directory it runs
    /// under.
    ///
    /// This is the supported way to reach the primary: the `Container` itself
    /// stays private, because the session owns it and will stop it at
    /// [`Outrig::shutdown`]. For a command you just want the output of, use
    /// [`Outrig::exec_capture`].
    pub async fn exec_stdio(&self, argv: &[String], options: &ExecOptions) -> Result<Child> {
        self.container.exec_stdio(argv, options).await
    }

    /// [`Outrig::exec_stdio`], driven to completion: stdout and stderr are
    /// drained concurrently and returned with the exit status. A non-zero
    /// exit is data on the returned [`Output`], not an error.
    pub async fn exec_capture(&self, argv: &[String], options: &ExecOptions) -> Result<Output> {
        self.container.exec_capture(argv, options).await
    }

    /// Dispatch an MCP `tools/call` to the named server. `server` must
    /// match a key in the effective MCP map; `tool` is the
    /// un-namespaced tool name as it appeared in [`Outrig::tools`].
    pub async fn call_tool(&self, server: &str, tool: &str, args: Value) -> Result<McpToolResult> {
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| OutrigError::Configuration(format!("no mcp server named {server:?}")))?;
        client.call_tool(tool, args).await
    }

    /// Shut down every MCP server (close-stdin -> 2 s grace -> SIGKILL),
    /// then detach the network interceptor from every container, stop the
    /// sidecars, and finally stop the primary. Errors during MCP and sidecar
    /// shutdown are logged and swallowed so a single misbehaving server or
    /// sidecar can't strand the rest; the primary `stop` error, if any,
    /// propagates.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            sidecars,
            container,
            clients,
            network,
            ..
        } = self;
        for (name, arc) in clients {
            match Arc::try_unwrap(arc) {
                Ok(client) => {
                    if let Err(e) = client.shutdown().await {
                        tracing::warn!(
                            target: "outrig::outrig",
                            "mcp shutdown for {name:?} failed: {e}"
                        );
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        target: "outrig::outrig",
                        "mcp client {name:?} still has outstanding refs at shutdown; relying on Drop"
                    );
                }
            }
        }
        if let Some(network) = network {
            network.shutdown().await;
        }
        for (name, sidecar) in sidecars {
            if let Err(e) = sidecar.stop(SHUTDOWN_GRACE).await {
                tracing::warn!(
                    target: "outrig::outrig",
                    "sidecar {name:?} stop failed: {e}"
                );
            }
        }
        container.stop(SHUTDOWN_GRACE).await
    }
}

/// Reject a [`SidecarSpec`] that could not start or would corrupt the
/// session's flat server namespace. Pure checks, no podman. Generic over
/// the map values so the checks are testable with plain name maps.
///
/// The name-shape, image-ref, and placement rules run through
/// `config::validate`, so a hand-built spec is refused by the same code, with
/// the same text, as the equivalent `[sidecars.<sc>]` block. Where those
/// messages name an image-config, this path supplies its podman image ref
/// instead -- a library spec has no image-config to name.
///
/// `helper_available` is [`enter::is_available`]'s answer, threaded in rather
/// than read here so the missing-`outrig-enter` rejection is testable on a
/// host that does have the helper.
fn validate_sidecar_spec<C, S>(
    spec: &SidecarSpec,
    existing_clients: &BTreeMap<String, C>,
    existing_sidecars: &BTreeMap<String, S>,
    helper_available: bool,
) -> Result<()> {
    check_sidecar_name(&spec.name)?;
    if existing_sidecars.contains_key(&spec.name) {
        return Err(OutrigError::Configuration(format!(
            "sidecar {:?} is already running",
            spec.name
        )));
    }
    check_sidecar_image(&spec.name, &spec.image)?;

    for (name, server) in &spec.servers {
        if !is_valid_mcp_server_name(name) {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?}: invalid mcp server name {name:?}",
                spec.name
            )));
        }
        if name == crate::tool_name::RESERVED_SERVER {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?}: mcp server name {name:?} is reserved for OutRig's \
                 built-in tools; its tools would collide with `{name}__*`",
                spec.name
            )));
        }
        if server.command().is_some_and(<[String]>::is_empty) {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?}: mcp server {name:?} has an empty command",
                spec.name
            )));
        }
        if existing_clients.contains_key(name) {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?}: mcp server name {name:?} is already connected in this \
                 session (the per-session server namespace is flat)",
                spec.name
            )));
        }
    }

    check_view_exclusions(
        &spec.name,
        spec.view,
        spec.workspace,
        spec.security.capabilities.profile,
    )?;
    let hosted: Vec<(&str, bool)> = spec
        .servers
        .iter()
        .map(|(name, server)| (name.as_str(), server.is_entrypoint()))
        .collect();
    check_entrypoint_hosting(&spec.image, &spec.name, spec.view, &hosted)?;

    // Checked after the placement rules so a spec that is wrong on its own
    // terms says so, rather than blaming a missing helper it would not have
    // used anyway.
    if spec.view == SidecarView::Primary && !helper_available {
        return Err(OutrigError::FilesystemHelperUnavailable);
    }
    Ok(())
}

/// Connect every server in `spec` against the started sidecar and index its
/// tools. On failure every client connected so far (including the failing
/// one, when it got that far) is shut down before the error propagates.
async fn connect_sidecar_servers(
    container: &Container,
    spec: &SidecarSpec,
    log_dir: &Path,
) -> Result<(BTreeMap<String, McpClient>, Vec<ToolHandle>)> {
    let mut clients: BTreeMap<String, McpClient> = BTreeMap::new();
    let mut tools: Vec<ToolHandle> = Vec::new();
    for (name, server) in &spec.servers {
        // An entrypoint server's container was created with its env and args
        // baked in; `podman start --attach` here is what finally runs it.
        let connected = match server.command() {
            Some(command) => {
                // Through the constructor rather than a `Full` literal, so a
                // field added to that variant is still filled in exactly one
                // place. Placement is already settled -- this server's
                // container is the one we were handed.
                let mcp_spec = McpServerSpec::exec(command).with_env(server.env().clone());
                McpClient::connect_via_podman_exec_with_source(
                    container,
                    &mcp_spec,
                    name,
                    McpDeclarationSource::LaunchSpec,
                    log_dir,
                    &BTreeMap::new(),
                )
                .await
            }
            None => {
                McpClient::connect_via_podman_start(
                    container,
                    name,
                    McpDeclarationSource::LaunchSpec,
                    log_dir,
                )
                .await
            }
        };
        let client = match connected {
            Ok(client) => client,
            Err(e) => {
                shutdown_partial_clients(clients).await;
                return Err(e);
            }
        };
        match client.list_tools().await {
            Ok(listed) => {
                tools.extend(tool_handles(name, listed));
                clients.insert(name.clone(), client);
            }
            Err(e) => {
                let _ = client.shutdown().await;
                shutdown_partial_clients(clients).await;
                return Err(e);
            }
        }
    }
    Ok((clients, tools))
}

/// Unwind helper for a failed [`Outrig::add_sidecar`].
async fn shutdown_partial_clients(clients: BTreeMap<String, McpClient>) {
    for (_, client) in clients {
        let _ = client.shutdown().await;
    }
}

/// Index one server's advertised tools as [`ToolHandle`]s.
fn tool_handles(server: &str, listed: Vec<crate::mcp::McpTool>) -> Vec<ToolHandle> {
    listed
        .into_iter()
        .map(|t| ToolHandle {
            server: server.to_string(),
            name: t.name,
            description: t.description.unwrap_or_default(),
            input_schema: t.input_schema,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_dir() -> PathBuf {
        PathBuf::from("logs")
    }

    #[test]
    fn from_image_defaults_to_merge_embedded_mcp() {
        let spec = LaunchSpec::from_image("img:latest", BTreeMap::new(), log_dir());

        assert_eq!(spec.embedded_mcp_policy, EmbeddedMcpPolicy::Merge);
    }

    #[test]
    fn builder_can_ignore_embedded_mcp() {
        let spec = LaunchSpec::from_image("img:latest", BTreeMap::new(), log_dir())
            .with_embedded_mcp_policy(EmbeddedMcpPolicy::Ignore);

        assert_eq!(spec.embedded_mcp_policy, EmbeddedMcpPolicy::Ignore);
    }

    fn tools_sidecar() -> SidecarSpec {
        SidecarSpec::from_image("tools", "ghcr.io/example/mcp-tools:1")
            .with_server("fs", vec!["mcp-fs".to_string(), "/workspace".to_string()])
    }

    #[test]
    fn sidecar_spec_builder_sets_fields() {
        let spec = tools_sidecar()
            .with_workspace_access(SidecarWorkspaceAccess::Ro)
            .with_mount(MountSpec::new(
                "/host/cache",
                "/cache",
                MountAccess::ReadWrite,
            ));

        assert_eq!(spec.name, "tools");
        assert_eq!(spec.image, "ghcr.io/example/mcp-tools:1");
        assert_eq!(spec.workspace, SidecarWorkspaceAccess::Ro);
        assert_eq!(spec.view, SidecarView::None);
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(
            spec.servers["fs"].command().expect("exec server")[0],
            "mcp-fs"
        );
    }

    #[test]
    fn sidecar_spec_builder_hosts_an_entrypoint_server() {
        let spec = SidecarSpec::from_image("fs", "docker.io/mcp/filesystem:latest")
            .with_entrypoint_server("fs", ["/workspace"])
            .with_view(SidecarView::Primary);

        assert_eq!(spec.view, SidecarView::Primary);
        let server = &spec.servers["fs"];
        assert!(server.is_entrypoint());
        assert_eq!(server.command(), None, "the image supplies the command");
        assert_eq!(server.args(), ["/workspace"]);
    }

    #[test]
    fn launch_spec_accumulates_sidecars() {
        let spec = LaunchSpec::from_image("img:latest", BTreeMap::new(), log_dir())
            .with_sidecar(tools_sidecar())
            .with_sidecar(SidecarSpec::from_image("grep", "img-grep:1"));

        let names: Vec<&str> = spec.sidecars.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["tools", "grep"]);
    }

    fn no_existing() -> (BTreeMap<String, ()>, BTreeMap<String, ()>) {
        (BTreeMap::new(), BTreeMap::new())
    }

    /// `validate_sidecar_spec` against an otherwise-empty session, on a host
    /// where `outrig-enter` is present.
    fn validate_alone(spec: &SidecarSpec) -> Result<()> {
        let (clients, sidecars) = no_existing();
        validate_sidecar_spec(spec, &clients, &sidecars, true)
    }

    /// The message the equivalent `[sidecars.<sc>]` block would produce, so
    /// the parity assertions below compare against the config path's own text
    /// rather than a hand-copied literal.
    fn config_error(toml_src: &str) -> String {
        let config: Config = toml::from_str(toml_src).expect("parse config");
        config
            .validate(None)
            .expect_err("config must fail validation")
            .to_string()
    }

    #[test]
    fn validate_accepts_a_plain_spec() {
        validate_alone(&tools_sidecar()).expect("spec is valid");
    }

    #[test]
    fn validate_rejects_bad_sidecar_name() {
        let err = validate_alone(&SidecarSpec::from_image("bad name!", "img:1"))
            .expect_err("space in name must fail");
        assert!(err.to_string().contains("invalid sidecar name"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_image() {
        let err =
            validate_alone(&SidecarSpec::from_image("t", "  ")).expect_err("blank image must fail");
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_server_command() {
        let spec = SidecarSpec::from_image("t", "img:1").with_server("fs", Vec::new());
        let err = validate_alone(&spec).expect_err("empty command must fail");
        assert!(err.to_string().contains("empty command"), "{err}");
    }

    #[test]
    fn validate_rejects_invalid_server_name() {
        let spec =
            SidecarSpec::from_image("t", "img:1").with_server("1bad", vec!["mcp-fs".to_string()]);
        let err = validate_alone(&spec).expect_err("digit-leading server name must fail");
        assert!(err.to_string().contains("invalid mcp server name"), "{err}");
    }

    #[test]
    fn validate_rejects_the_reserved_server_name() {
        let spec = SidecarSpec::from_image("t", "img:1")
            .with_server(crate::RESERVED_SERVER, vec!["mcp-fs".to_string()]);
        let err = validate_alone(&spec).expect_err("the built-in tool namespace is reserved");
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn validate_rejects_server_name_collision() {
        let (mut clients, sidecars) = no_existing();
        clients.insert("fs".to_string(), ());
        let err = validate_sidecar_spec(&tools_sidecar(), &clients, &sidecars, true)
            .expect_err("flat namespace collision must fail");
        assert!(err.to_string().contains("already connected"), "{err}");
    }

    #[test]
    fn validate_rejects_running_sidecar_name() {
        let (clients, mut sidecars) = no_existing();
        sidecars.insert("tools".to_string(), ());
        let err = validate_sidecar_spec(&tools_sidecar(), &clients, &sidecars, true)
            .expect_err("duplicate sidecar name must fail");
        assert!(err.to_string().contains("already running"), "{err}");
    }

    fn view_sidecar() -> SidecarSpec {
        SidecarSpec::from_image("fs", "docker.io/mcp/filesystem:latest")
            .with_view(SidecarView::Primary)
            .with_entrypoint_server("fs", ["/"])
    }

    #[test]
    fn validate_accepts_a_primary_view_entrypoint_spec() {
        validate_alone(&view_sidecar()).expect("spec is valid");
    }

    /// [`config_error`] for the `[sidecars.fs]` block [`view_sidecar`] mirrors,
    /// with `tail` appended -- so each parity test below shows only the key
    /// that makes its case.
    fn view_config_error(tail: &str) -> String {
        config_error(&format!(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
ws = {{ sidecar = "fs" }}
[sidecars.fs]
image = "docker.io/mcp/filesystem:latest"
view = "primary"
{tail}
"#
        ))
    }

    #[test]
    fn validate_rejects_primary_view_with_workspace() {
        let err = validate_alone(&view_sidecar().with_workspace_access(SidecarWorkspaceAccess::Ro))
            .expect_err("view + workspace are mutually exclusive");
        assert_eq!(
            err.to_string(),
            view_config_error(r#"workspace = "ro""#),
            "the two paths must produce the same text from the same code"
        );
    }

    #[test]
    fn validate_rejects_primary_view_with_dropped_caps() {
        let spec = view_sidecar().with_security(SecuritySpec {
            capabilities: CapabilitySpec::new(CapabilityProfile::DropAll),
            ..SecuritySpec::default()
        });
        let err = validate_alone(&spec).expect_err("the view needs SYS_ADMIN / SYS_PTRACE");
        assert_eq!(
            err.to_string(),
            view_config_error("[sidecars.fs.security]\ncapability-profile = \"drop-all\""),
            "the two paths must produce the same text from the same code"
        );
    }

    #[test]
    fn validate_rejects_entrypoint_host_with_a_second_server() {
        let spec = SidecarSpec::from_image("fs", "img:1")
            .with_entrypoint_server("fs", ["/"])
            .with_server("extra", vec!["mcp-extra".to_string()]);
        let err = validate_alone(&spec).expect_err("an entrypoint host serves exactly one");
        assert!(
            err.to_string().contains(
                "the container process is the server, so an entrypoint host serves \
                           exactly one"
            ),
            "{err}"
        );
    }

    #[test]
    fn validate_rejects_primary_view_over_an_exec_server() {
        let spec = SidecarSpec::from_image("fs", "img:1")
            .with_view(SidecarView::Primary)
            .with_server("fs", vec!["mcp-fs".to_string()]);
        let err = validate_alone(&spec).expect_err("the view is entrypoint-stdio only");
        assert!(
            err.to_string().contains("it is entrypoint-stdio only"),
            "{err}"
        );
    }

    /// A build without the musl target embeds no launcher, so a
    /// `view = "primary"` spec has to fail before any container exists --
    /// naming the artifact rather than surfacing an opaque podman error.
    #[test]
    fn validate_rejects_primary_view_without_the_helper() {
        let (clients, sidecars) = no_existing();
        let err = validate_sidecar_spec(&view_sidecar(), &clients, &sidecars, false)
            .expect_err("no launcher, no view");
        let msg = err.to_string();
        assert!(msg.contains("filesystem-view helper"), "{msg}");
        assert!(msg.contains("unknown-linux-musl"), "{msg}");
    }

    /// The helper check runs after the placement rules, so a spec that is
    /// wrong on its own terms says so rather than blaming the toolchain.
    #[test]
    fn placement_errors_outrank_a_missing_helper() {
        let (clients, sidecars) = no_existing();
        let spec = view_sidecar().with_workspace_access(SidecarWorkspaceAccess::Ro);
        let err = validate_sidecar_spec(&spec, &clients, &sidecars, false)
            .expect_err("spec is invalid either way");
        assert!(err.to_string().contains("mutually exclusive"), "{err}");
    }

    /// Translate the `[images.primary]` block of `toml_src` the way
    /// `from_config` does, minus the async image resolution -- so sidecar
    /// images stay their unresolved config refs.
    fn launch_parts(toml_src: &str) -> (BTreeMap<String, McpServerSpec>, Vec<SidecarSpec>) {
        let config: Config = toml::from_str(toml_src).expect("parse config");
        let cfg = config.images.get("primary").expect("primary image-config");
        plan_to_launch_parts(&sidecar::plan_from_config(&config, cfg), Path::new("/repo"))
    }

    #[test]
    fn from_config_keeps_primary_servers_in_mcp() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fs = { command = ["mcp-fs", "/workspace"] }
shell = ["bash", "-lc", "sh"]
"#,
        );

        let names: Vec<&str> = mcp.keys().map(|k| k.as_str()).collect();
        assert_eq!(names, ["fs", "shell"]);
        assert!(sidecars.is_empty(), "no placement -> no sidecars");
    }

    #[test]
    fn from_config_translates_named_sidecar() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fs = { command = ["mcp-fs"] }
lint = { command = ["mcp-lint", "--stdio"], sidecar = "tools", env = { LINT = "1" } }
[sidecars.tools]
image = "mcp-tools"
workspace = "ro"
[[sidecars.tools.mounts]]
host-path = "cache"
container-path = "/cache"
access = "read-write"
[sidecars.tools.security]
capability-profile = "no-net-raw"
"#,
        );

        let primary: Vec<&str> = mcp.keys().map(|k| k.as_str()).collect();
        assert_eq!(primary, ["fs"], "placed server left the primary map");

        assert_eq!(sidecars.len(), 1);
        let tools = &sidecars[0];
        assert_eq!(tools.name, "tools");
        assert_eq!(
            tools.image, "mcp-tools",
            "image ref stays unresolved for the caller"
        );
        assert_eq!(tools.workspace, SidecarWorkspaceAccess::Ro);
        assert_eq!(tools.mounts.len(), 1);
        assert_eq!(tools.mounts[0].host, PathBuf::from("/repo/cache"));
        assert_eq!(
            tools.security.capabilities.profile,
            CapabilityProfile::NoNetRaw
        );
        let lint = &tools.servers["lint"];
        assert_eq!(
            lint.command(),
            Some(&["mcp-lint".into(), "--stdio".into()][..])
        );
        assert!(lint.env().contains_key("LINT"));
    }

    #[test]
    fn from_config_translates_anonymous_exec_sidecar() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
grep = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
"#,
        );

        assert!(mcp.is_empty(), "anonymous server is sidecar-placed");
        assert_eq!(sidecars.len(), 1);
        assert_eq!(sidecars[0].name, "grep");
        assert_eq!(sidecars[0].image, "ghcr.io/example/mcp-grep:1");
        assert_eq!(
            sidecars[0].servers["grep"].command(),
            Some(&["mcp-grep".into()][..])
        );
    }

    #[test]
    fn from_config_skips_manual_sidecars() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
lint = { command = ["mcp-lint"], sidecar = "tools" }
[sidecars.tools]
image = "mcp-tools"
start = "manual"
"#,
        );

        assert!(
            mcp.is_empty(),
            "manual sidecar's server is not primary-hosted"
        );
        assert!(
            sidecars.is_empty(),
            "manual sidecar is not started at launch"
        );
    }

    #[test]
    fn from_config_lowers_inline_entrypoint_stdio() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fetch = { image = "ghcr.io/example/mcp-fetch:2", args = ["/workspace"], env = { TOKEN = "abc" } }
"#,
        );

        assert!(mcp.is_empty(), "entrypoint server is sidecar-placed");
        assert_eq!(sidecars.len(), 1);
        let fetch = &sidecars[0];
        assert_eq!(
            fetch.name, "fetch",
            "anonymous sidecar takes its server's name"
        );
        assert_eq!(fetch.image, "ghcr.io/example/mcp-fetch:2");
        assert_eq!(fetch.view, SidecarView::None);

        let server = &fetch.servers["fetch"];
        assert!(
            server.is_entrypoint(),
            "no command -> the ENTRYPOINT is the server"
        );
        assert_eq!(server.command(), None);
        assert_eq!(server.args(), ["/workspace"]);
        assert!(server.env().contains_key("TOKEN"));
    }

    /// `args` on the `[sidecars.<sc>]` block reaches the spec just as an
    /// entry's own does -- `entrypoint_args` picks between them at the
    /// lowering boundary, so the spec carries one unambiguous slot.
    #[test]
    fn from_config_lowers_block_args_for_named_entrypoint_host() {
        let (_, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
ws = { sidecar = "served" }
[sidecars.served]
image = "entry:1"
workspace = "ro"
args = ["/workspace"]
"#,
        );

        let served = &sidecars[0];
        assert_eq!(served.workspace, SidecarWorkspaceAccess::Ro);
        assert_eq!(served.servers["ws"].args(), ["/workspace"]);
    }

    /// A config-declared entrypoint sidecar and the hand-built spec a library
    /// caller would write for it lower to the same thing, which is what makes
    /// the two paths produce the same containers, argv, and labels downstream:
    /// everything after this point -- `sidecar_container_name`, the label map,
    /// `entrypoint_create_args` -- reads only these fields.
    #[test]
    fn config_and_hand_built_entrypoint_specs_agree() {
        let (_, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
ws = { sidecar = "served" }
[sidecars.served]
image = "entry:1"
workspace = "ro"
args = ["/workspace"]
"#,
        );
        let lowered = &sidecars[0];
        let hand_built = SidecarSpec::from_image("served", "entry:1")
            .with_workspace_access(SidecarWorkspaceAccess::Ro)
            .with_entrypoint_server("ws", ["/workspace"]);

        assert_eq!(lowered.name, hand_built.name);
        assert_eq!(lowered.image, hand_built.image);
        assert_eq!(lowered.workspace, hand_built.workspace);
        assert_eq!(lowered.view, hand_built.view);
        assert_eq!(lowered.mounts.len(), hand_built.mounts.len());
        assert_eq!(
            lowered.servers.keys().collect::<Vec<_>>(),
            hand_built.servers.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            lowered.servers["ws"].args(),
            hand_built.servers["ws"].args()
        );
        assert_eq!(
            lowered.servers["ws"].command(),
            hand_built.servers["ws"].command()
        );
    }

    #[test]
    fn from_config_lowers_primary_view() {
        let (_, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/"] }
"#,
        );

        assert_eq!(sidecars[0].view, SidecarView::Primary);
        assert_eq!(sidecars[0].servers["fs"].args(), ["/"]);
    }

    #[test]
    fn from_config_partitions_mixed_placements() {
        let (mcp, sidecars) = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fs = { command = ["mcp-fs"] }
lint = { command = ["mcp-lint"], sidecar = "tools" }
grep = { command = ["mcp-grep"], image = "grep:1" }
slow = { command = ["mcp-slow"], sidecar = "later" }
[sidecars.tools]
image = "mcp-tools"
[sidecars.later]
image = "mcp-later"
start = "manual"
"#,
        );

        let primary: Vec<&str> = mcp.keys().map(|k| k.as_str()).collect();
        assert_eq!(primary, ["fs"], "only the unplaced server stays primary");
        let names: Vec<&str> = sidecars.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["grep", "tools"],
            "manual `later` dropped; BTreeMap name order"
        );
    }
}
