//! Curated facade for library callers. Composes existing primitives
//! (`image::ensure_image`, `Container`, `McpClient`) into a single
//! `launch -> tools/call_tool -> shutdown` flow that doesn't expose
//! internals like rig, the REPL, or session storage.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::{ContainerConfig, EnvValue, McpServerSpec, Workspace};
use crate::container::{Container, embedded};
use crate::error::{OutrigError, Result};
use crate::image::{self, ImageTag};
use crate::mcp::{McpClient, McpToolResult};

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

/// Description of one container launch: image source, optional workspace
/// mount, MCP servers to start inside, and the directory to land per-server
/// stderr in.
pub struct LaunchSpec {
    pub(crate) source: LaunchSource,
    pub workspace: Option<WorkspaceSpec>,
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
            mcp,
            log_dir,
        }
    }

    /// Build a `LaunchSpec` from a parsed `[containers.<name>]` block plus
    /// the project's `[workspace]`. Resolves repo-relative paths against
    /// `repo_root` so the resulting spec carries absolute paths and is
    /// independent of the caller's current directory.
    pub fn from_container_config(
        cfg: &ContainerConfig,
        workspace: &Workspace,
        repo_root: &Path,
        log_dir: PathBuf,
    ) -> Self {
        let dockerfile = repo_root.join(&cfg.dockerfile);
        let context = repo_root.join(&cfg.context);
        let host = if workspace.host_path.is_absolute() {
            workspace.host_path.clone()
        } else {
            repo_root.join(&workspace.host_path)
        };
        Self {
            source: LaunchSource::Build {
                dockerfile,
                context,
                build_args: cfg.build_args.clone(),
            },
            workspace: Some(WorkspaceSpec {
                host,
                container: workspace.container_path.clone(),
            }),
            mcp: cfg.mcp.clone(),
            log_dir,
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
}

impl Outrig {
    /// Acquire the image, start the container, bootstrap the runtime user,
    /// merge image-embedded MCP config with `spec.mcp`, connect every merged
    /// MCP server, and index their tools.
    /// Returns once every server has answered an initial `tools/list`.
    pub async fn launch(spec: &LaunchSpec) -> Result<Self> {
        let image_tag = match &spec.source {
            LaunchSource::Build {
                dockerfile,
                context,
                build_args,
            } => {
                // Reuse `ensure_image` by wrapping the raw inputs in a
                // `ContainerConfig`. Passing an empty `repo_root` makes
                // its `repo_root.join(absolute)` calls no-ops.
                let cfg = ContainerConfig {
                    dockerfile: dockerfile.clone(),
                    context: context.clone(),
                    build_args: build_args.clone(),
                    mcp: BTreeMap::new(),
                };
                image::ensure_image(&cfg, Path::new(""), false).await?.tag
            }
            LaunchSource::Image { tag } => ImageTag(tag.clone()),
        };

        let workspace = spec
            .workspace
            .as_ref()
            .map(|w| (w.host.as_path(), w.container.as_path()));
        let mut container = Container::start(&image_tag, workspace).await?;
        container.bootstrap_user().await?;

        let mcp = embedded::merged_mcp(&container, &spec.mcp).await?;

        let mut clients: BTreeMap<String, Arc<McpClient>> = BTreeMap::new();
        let mut tools: Vec<ToolHandle> = Vec::new();
        for (name, server_cfg) in &mcp {
            let client =
                McpClient::connect_via_podman_exec(&container, server_cfg, name, &spec.log_dir)
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
        })
    }

    /// Tools advertised by every connected MCP server, in `(server, name)`
    /// order matching the `BTreeMap` iteration of `spec.mcp` followed by
    /// each server's advertised order.
    pub fn tools(&self) -> &[ToolHandle] {
        &self.tools
    }

    /// Dispatch an MCP `tools/call` to the named server. `server` must
    /// match a key in the `LaunchSpec::mcp` map; `tool` is the
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
            container, clients, ..
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
        container.stop(SHUTDOWN_GRACE).await
    }
}
