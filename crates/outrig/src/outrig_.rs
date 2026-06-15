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
    CapabilityProfile, ContainerSecurity, EnvValue, ImageConfig, ImageSourceRef, McpServerSpec,
    MountAccess, NetworkMode, NetworkPolicy, Workspace,
};
use crate::container::{
    Container, ContainerCapabilities, ContainerLaunchSpec, ContainerMount, ContainerWorkspace,
    embedded::{self, McpDeclarationSource},
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
#[derive(Debug, Clone, Default)]
pub struct SecuritySpec {
    pub capabilities: CapabilitySpec,
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
            log_dir,
        }
    }

    /// Build a `LaunchSpec` from a parsed `[images.<name>]` block plus
    /// the project's `[workspace]`. Resolves repo-relative paths against
    /// `repo_root` so the resulting spec carries absolute paths and is
    /// independent of the caller's current directory.
    pub fn from_image_config(
        cfg: &ImageConfig,
        workspace: &Workspace,
        repo_root: &Path,
        log_dir: PathBuf,
    ) -> Self {
        let host = resolve_workspace_host(repo_root, &workspace.host_path);
        let ws = WorkspaceSpec {
            host,
            container: workspace.container_path.clone(),
        };
        let mounts = workspace
            .mounts
            .iter()
            .map(|mount| MountSpec {
                host: resolve_workspace_host(repo_root, &mount.host_path),
                container: mount.container_path.clone(),
                access: mount.access,
            })
            .collect();
        match cfg.source() {
            ImageSourceRef::Build {
                dockerfile,
                context,
                build_args,
            } => Self {
                source: LaunchSource::Build {
                    dockerfile: repo_root.join(dockerfile),
                    context: repo_root.join(context),
                    build_args: build_args.clone(),
                },
                workspace: Some(ws),
                mounts,
                security: SecuritySpec::from(&cfg.security),
                network: NetworkSpec::default(),
                embedded_mcp_policy: EmbeddedMcpPolicy::default(),
                mcp: cfg.mcp.clone(),
                log_dir,
            },
            ImageSourceRef::Image { image_name } => Self {
                source: LaunchSource::Image {
                    tag: image_name.to_string(),
                },
                workspace: Some(ws),
                mounts,
                security: SecuritySpec::from(&cfg.security),
                network: NetworkSpec::default(),
                embedded_mcp_policy: EmbeddedMcpPolicy::default(),
                mcp: cfg.mcp.clone(),
                log_dir,
            },
        }
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
}

fn resolve_workspace_host(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
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

/// A running container with a set of MCP servers attached. Construct via
/// [`Outrig::launch`]; clean up via [`Outrig::shutdown`]. Dropping without
/// `shutdown` still removes the container via a detached `podman rm -f`
/// (plus the panic-hook sweeper if the host installed it).
pub struct Outrig {
    container: Container,
    clients: BTreeMap<String, Arc<McpClient>>,
    tools: Vec<ToolHandle>,
    network: Option<NetworkInterceptor>,
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
            for t in client.list_tools().await? {
                tools.push(ToolHandle {
                    server: name.clone(),
                    name: t.name,
                    description: t.description.unwrap_or_default(),
                    input_schema: t.input_schema,
                });
            }
            clients.insert(name.clone(), Arc::new(client));
        }

        Ok(Self {
            container,
            clients,
            tools,
            network,
        })
    }

    /// Tools advertised by every connected MCP server, in `(server, name)`
    /// order matching the effective MCP map's `BTreeMap` iteration followed by
    /// each server's advertised order.
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
    /// then stop the container. Errors during MCP shutdown are logged and
    /// swallowed so a single misbehaving server can't strand the
    /// container; the container `stop` error, if any, propagates.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
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
        container.stop(SHUTDOWN_GRACE).await
    }
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
}
