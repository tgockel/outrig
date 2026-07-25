//! Curated facade for library callers. Composes existing primitives
//! (`image::ensure_image`, `Container`, `McpClient`) into a single
//! `launch -> tools/call_tool -> shutdown` flow that doesn't expose
//! internals like rig, the REPL, or session storage.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::{
    CapabilityProfile, Config, ContainerSecurity, EnvValue, ImageConfig, ImageSourceRef,
    McpServerSpec, MountAccess, NetworkMode, NetworkPolicy, SidecarStart, SidecarWorkspaceAccess,
    is_valid_mcp_server_name, is_valid_sidecar_name,
};
use crate::container::{
    Container, ContainerCapabilities, ContainerLaunchSpec, ContainerMount, ContainerWorkspace,
    LABEL_SESSION, LABEL_SIDECAR,
    embedded::{self, McpDeclarationSource},
    sidecar::{self, Placement, SessionMcpPlan},
};
use crate::error::{OutrigError, Result};
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
pub struct WorkspaceSpec {
    pub host: PathBuf,
    pub container: PathBuf,
}

/// Extra host directory mounted into the container.
#[derive(Debug, Clone)]
pub struct MountSpec {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

/// Container security policy applied at launch.
#[derive(Debug, Clone)]
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
pub struct CapabilitySpec {
    pub profile: CapabilityProfile,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
}

/// Network monitoring policy applied at launch.
#[derive(Debug, Clone, Default)]
pub struct NetworkSpec {
    pub mode: NetworkMode,
    pub policy: Option<NetworkPolicy>,
}

/// One MCP server hosted by a [`SidecarSpec`] sidecar. Exec-stdio only:
/// the server is spawned with `podman exec -i` inside the sidecar, so a
/// command is always required (entrypoint-stdio has no library surface).
#[derive(Debug, Clone)]
pub struct SidecarServerSpec {
    pub command: Vec<String>,
    pub env: BTreeMap<String, EnvValue>,
}

/// Description of one sidecar container: image (a raw podman ref, used
/// verbatim like [`LaunchSpec::from_image`]), workspace visibility, extra
/// mounts, security policy, and the MCP servers it hosts. Passed to
/// [`Outrig::add_sidecar`] mid-session or declared at launch via
/// [`LaunchSpec::with_sidecar`].
///
/// Config-name image resolution (the `[images.<name>]` lookup the CLI
/// performs) is out of facade scope: callers who want a built image run
/// `image::ensure_image` themselves and pass the resulting tag.
#[derive(Debug, Clone)]
pub struct SidecarSpec {
    pub name: String,
    pub(crate) image: String,
    pub workspace: SidecarWorkspaceAccess,
    pub mounts: Vec<MountSpec>,
    pub security: SecuritySpec,
    pub servers: BTreeMap<String, SidecarServerSpec>,
}

impl SidecarSpec {
    /// A sidecar named `name` running `image` (a podman ref used verbatim,
    /// no build or pull): no workspace view, no mounts, default security,
    /// no servers.
    pub fn from_image(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            workspace: SidecarWorkspaceAccess::None,
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
        mut self,
        name: impl Into<String>,
        command: Vec<String>,
        env: BTreeMap<String, EnvValue>,
    ) -> Self {
        self.servers
            .insert(name.into(), SidecarServerSpec { command, env });
        self
    }
}

/// How [`Outrig::launch`] handles MCP servers declared in an image's
/// `org.outrig.mcp` label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

/// Description of one container launch: image source, optional workspace
/// mount, MCP servers to start inside, and the directory to land per-server
/// stderr in.
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
    /// placement into sidecars: top-level `[sidecars.<sc>]` blocks and
    /// placement-bearing entries (`sidecar = "<sc>"`, inline `image = "..."`
    /// with a `command`) become [`SidecarSpec`]s on the returned spec, so
    /// [`Outrig::launch`] no longer rejects them. Named-sidecar and anonymous
    /// images resolve like `--image` (a sibling `[images.<name>]` block first,
    /// else a raw local ref) and are **built/pulled eagerly here**; the primary
    /// image is still built lazily by `launch`.
    ///
    /// Scope of the library translation (the CLI path is a strict superset):
    /// - `start = "manual"` sidecars are skipped -- neither started nor carried.
    ///   Rebuild a [`SidecarSpec`] and call [`Outrig::add_sidecar`] to start one
    ///   mid-session.
    /// - Entrypoint-stdio placements (a placed entry with no `command`, whether
    ///   it names an inline `image` or a `sidecar` block) have no exec-stdio
    ///   library form and are rejected with an error; run those via the CLI
    ///   (`outrig run` / `outrig mcp`).
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

        let ws = WorkspaceSpec {
            host: resolve_workspace_host(repo_root, &config.workspace.host_path),
            container: config.workspace.container_path.clone(),
        };
        let mounts = config
            .workspace
            .mounts
            .iter()
            .map(|mount| MountSpec {
                host: resolve_workspace_host(repo_root, &mount.host_path),
                container: mount.container_path.clone(),
                access: mount.access,
            })
            .collect();
        let source = match cfg.source() {
            ImageSourceRef::Build {
                dockerfile,
                context,
                build_args,
            } => LaunchSource::Build {
                dockerfile: repo_root.join(dockerfile),
                context: repo_root.join(context),
                build_args: build_args.clone(),
            },
            ImageSourceRef::Image { image_name } => LaunchSource::Image {
                tag: image_name.to_string(),
            },
        };

        let plan = sidecar::plan_from_config(config, cfg);
        let (mcp, mut sidecars) = plan_to_launch_parts(&plan, repo_root)?;
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

fn resolve_workspace_host(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

/// Split a planned session into the primary MCP map and the launch-time
/// sidecars. Pure -- no image resolution, so each `SidecarSpec.image` still
/// holds the unresolved config ref for [`LaunchSpec::from_config`] to rewrite.
/// `start = "manual"` sidecars are dropped (their servers, being sidecar-placed,
/// are already absent from the primary map); an entrypoint-stdio placement has
/// no exec-stdio library form and is an error.
fn plan_to_launch_parts(
    plan: &SessionMcpPlan,
    repo_root: &Path,
) -> Result<(BTreeMap<String, McpServerSpec>, Vec<SidecarSpec>)> {
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
        if let Some((server, _)) = plan.entrypoint_server_in(sc) {
            return Err(OutrigError::Configuration(format!(
                "mcp server {server:?} is entrypoint-stdio (a placed entry with no command, \
                 so the container's ENTRYPOINT is the server); the library facade hosts \
                 exec-stdio servers only -- run it via the CLI (outrig run / outrig mcp) or \
                 give the server a command"
            )));
        }
        let servers = plan
            .servers_in(&sc.name)
            .map(|(name, placed)| {
                let (command, env) = placed.spec.normalize();
                (name.clone(), SidecarServerSpec { command, env })
            })
            .collect();
        let mounts = sc
            .mounts
            .iter()
            .map(|mount| MountSpec {
                host: resolve_workspace_host(repo_root, &mount.host_path),
                container: mount.container_path.clone(),
                access: mount.access,
            })
            .collect();
        sidecars.push(SidecarSpec {
            name: sc.name.clone(),
            image: sc.image.clone(),
            workspace: sc.workspace,
            mounts,
            security: SecuritySpec::from(&sc.security),
            servers,
        });
    }
    Ok((mcp, sidecars))
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
            Ok(tag.0)
        }
        None => {
            let tag = ImageTag(image_ref.to_string());
            image::ensure_local_image(&tag, None).await?;
            Ok(tag.0)
        }
    }
}

/// Description of a single tool exposed by one of the running MCP servers.
/// `server` is the local config name (`spec.mcp` key) and `name` is the
/// tool name as advertised by the server (un-namespaced).
#[derive(Debug, Clone)]
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
                // `ImageConfig`. Passing an empty `repo_root` makes
                // its `repo_root.join(absolute)` calls no-ops.
                let cfg = ImageConfig {
                    image_name: None,
                    dockerfile: Some(dockerfile.clone()),
                    context: Some(context.clone()),
                    build_args: build_args.clone(),
                    security: ContainerSecurity::default(),
                    mcp: BTreeMap::new(),
                };
                image::ensure_image(&cfg, Path::new(""), false).await?.tag
            }
            LaunchSource::Image { tag } => ImageTag(tag.clone()),
        };

        let launch = ContainerLaunchSpec {
            workspace: spec.workspace.as_ref().map(|workspace| ContainerWorkspace {
                host: workspace.host.clone(),
                container: workspace.container.clone(),
                access: MountAccess::ReadWrite,
            }),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| ContainerMount {
                    host: mount.host.clone(),
                    container: mount.container.clone(),
                    access: mount.access,
                })
                .collect(),
            capabilities: ContainerCapabilities::from(&spec.security.capabilities),
            devices: spec.security.devices.clone(),
            no_new_privileges: spec.security.no_new_privileges,
            labels: BTreeMap::new(),
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
    /// launched with one), and one exec-stdio MCP connection per server in
    /// the spec. On success the new servers' tools are appended to
    /// [`Outrig::tools`] and returned. On failure everything started by this
    /// call is torn down (clients, interceptor attachment, container) and the
    /// session is left exactly as it was -- errors reach only the caller.
    pub async fn add_sidecar(&mut self, spec: SidecarSpec) -> Result<Vec<ToolHandle>> {
        validate_sidecar_spec(&spec, &self.clients, &self.sidecars)?;

        let workspace_access = spec.workspace.mount_access();
        if workspace_access.is_some() && self.container.host_workspace().as_os_str().is_empty() {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?} requests workspace access but the session has no workspace",
                spec.name
            )));
        }
        let launch = ContainerLaunchSpec {
            workspace: workspace_access.map(|access| ContainerWorkspace {
                host: self.container.host_workspace().to_path_buf(),
                container: self.container.container_workspace().to_path_buf(),
                access,
            }),
            mounts: spec
                .mounts
                .iter()
                .map(|mount| ContainerMount {
                    host: mount.host.clone(),
                    container: mount.container.clone(),
                    access: mount.access,
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
        };
        let container_name =
            crate::container::sidecar_container_name(self.container.session_suffix(), &spec.name);

        let image = ImageTag(spec.image.clone());
        let mut container = Container::start_named(&image, launch, container_name, None).await?;

        // Bootstrap only where identity matters; every SidecarSpec server is
        // exec-stdio, so any server implies bootstrap.
        let needs_bootstrap = crate::container::sidecar::bootstrap_needed(
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

    /// Tools advertised by every connected MCP server: launch-time servers in
    /// `(server, name)` order matching the effective MCP map's `BTreeMap`
    /// iteration followed by each server's advertised order, then each
    /// [`Outrig::add_sidecar`]'s tools in call order.
    pub fn tools(&self) -> &[ToolHandle] {
        &self.tools
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
fn validate_sidecar_spec<C, S>(
    spec: &SidecarSpec,
    existing_clients: &BTreeMap<String, C>,
    existing_sidecars: &BTreeMap<String, S>,
) -> Result<()> {
    if !is_valid_sidecar_name(&spec.name) {
        return Err(OutrigError::Configuration(format!(
            "invalid sidecar name {:?} (must match ^[A-Za-z0-9][A-Za-z0-9_-]*$ -- it \
             embeds in container names)",
            spec.name
        )));
    }
    if existing_sidecars.contains_key(&spec.name) {
        return Err(OutrigError::Configuration(format!(
            "sidecar {:?} is already running",
            spec.name
        )));
    }
    if spec.image.trim().is_empty() {
        return Err(OutrigError::Configuration(format!(
            "sidecar {:?} has an empty image ref",
            spec.name
        )));
    }
    for (name, server) in &spec.servers {
        if !is_valid_mcp_server_name(name) {
            return Err(OutrigError::Configuration(format!(
                "sidecar {:?}: invalid mcp server name {name:?}",
                spec.name
            )));
        }
        if server.command.is_empty() {
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
        let mcp_spec = McpServerSpec::Full {
            command: Some(server.command.clone()),
            env: server.env.clone(),
            sidecar: None,
            image: None,
            // exec-stdio: `command` is the full argv.
            args: Vec::new(),
        };
        let client = match McpClient::connect_via_podman_exec_with_source(
            container,
            &mcp_spec,
            name,
            McpDeclarationSource::LaunchSpec,
            log_dir,
            &BTreeMap::new(),
        )
        .await
        {
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
            .with_mount(MountSpec {
                host: PathBuf::from("/host/cache"),
                container: PathBuf::from("/cache"),
                access: MountAccess::ReadWrite,
            });

        assert_eq!(spec.name, "tools");
        assert_eq!(spec.image, "ghcr.io/example/mcp-tools:1");
        assert_eq!(spec.workspace, SidecarWorkspaceAccess::Ro);
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.servers["fs"].command[0], "mcp-fs");
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

    #[test]
    fn validate_accepts_a_plain_spec() {
        let (clients, sidecars) = no_existing();
        validate_sidecar_spec(&tools_sidecar(), &clients, &sidecars).expect("spec is valid");
    }

    #[test]
    fn validate_rejects_bad_sidecar_name() {
        let (clients, sidecars) = no_existing();
        let err = validate_sidecar_spec(
            &SidecarSpec::from_image("bad name!", "img:1"),
            &clients,
            &sidecars,
        )
        .expect_err("space in name must fail");
        assert!(err.to_string().contains("invalid sidecar name"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_image() {
        let (clients, sidecars) = no_existing();
        let err = validate_sidecar_spec(&SidecarSpec::from_image("t", "  "), &clients, &sidecars)
            .expect_err("blank image must fail");
        assert!(err.to_string().contains("empty image ref"), "{err}");
    }

    #[test]
    fn validate_rejects_empty_server_command() {
        let (clients, sidecars) = no_existing();
        let spec = SidecarSpec::from_image("t", "img:1").with_server("fs", Vec::new());
        let err =
            validate_sidecar_spec(&spec, &clients, &sidecars).expect_err("empty command must fail");
        assert!(err.to_string().contains("empty command"), "{err}");
    }

    #[test]
    fn validate_rejects_invalid_server_name() {
        let (clients, sidecars) = no_existing();
        let spec =
            SidecarSpec::from_image("t", "img:1").with_server("1bad", vec!["mcp-fs".to_string()]);
        let err = validate_sidecar_spec(&spec, &clients, &sidecars)
            .expect_err("digit-leading server name must fail");
        assert!(err.to_string().contains("invalid mcp server name"), "{err}");
    }

    #[test]
    fn validate_rejects_server_name_collision() {
        let (mut clients, sidecars) = no_existing();
        clients.insert("fs".to_string(), ());
        let err = validate_sidecar_spec(&tools_sidecar(), &clients, &sidecars)
            .expect_err("flat namespace collision must fail");
        assert!(err.to_string().contains("already connected"), "{err}");
    }

    #[test]
    fn validate_rejects_running_sidecar_name() {
        let (clients, mut sidecars) = no_existing();
        sidecars.insert("tools".to_string(), ());
        let err = validate_sidecar_spec(&tools_sidecar(), &clients, &sidecars)
            .expect_err("duplicate sidecar name must fail");
        assert!(err.to_string().contains("already running"), "{err}");
    }

    /// Translate the `[images.primary]` block of `toml_src` the way
    /// `from_config` does, minus the async image resolution -- so sidecar
    /// images stay their unresolved config refs.
    fn launch_parts(toml_src: &str) -> Result<(BTreeMap<String, McpServerSpec>, Vec<SidecarSpec>)> {
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
        )
        .expect("translation succeeds");

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
        )
        .expect("translation succeeds");

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
        assert_eq!(lint.command, ["mcp-lint", "--stdio"]);
        assert!(lint.env.contains_key("LINT"));
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
        )
        .expect("translation succeeds");

        assert!(mcp.is_empty(), "anonymous server is sidecar-placed");
        assert_eq!(sidecars.len(), 1);
        assert_eq!(sidecars[0].name, "grep");
        assert_eq!(sidecars[0].image, "ghcr.io/example/mcp-grep:1");
        assert_eq!(sidecars[0].servers["grep"].command, ["mcp-grep"]);
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
        )
        .expect("translation succeeds");

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
    fn from_config_rejects_entrypoint_stdio() {
        let err = launch_parts(
            r#"
[images.primary]
image-name = "primary:latest"
[images.primary.mcp]
fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "abc" } }
"#,
        )
        .expect_err("entrypoint-stdio has no library form");
        let msg = err.to_string();
        assert!(msg.contains("fetch"), "{msg}");
        assert!(msg.contains("entrypoint-stdio"), "{msg}");
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
        )
        .expect("translation succeeds");

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
