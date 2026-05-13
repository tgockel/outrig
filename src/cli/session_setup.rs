//! Shared bootstrap for `outrig run` and (future) `outrig mcp`. Lifts the
//! sequence of "load + merge config -> resolve container -> ensure image ->
//! start + bootstrap container -> session row + log dir" out of [`run`]
//! so both subcommands hit the same code path.
//!
//! Three pieces:
//!
//! - [`setup`] -- everything from config-load through "container started +
//!   bootstrapped + session row + log dir created", returning a populated
//!   [`SessionSetup`]. Stops *before* MCP children connect.
//! - [`merged_mcp`] + [`connect_mcp_clients`] -- reads any image-embedded MCP
//!   config, applies repo-config overrides, and spawns one [`McpClient`] per
//!   merged backing MCP in `BTreeMap` (key-sorted, deterministic) iteration
//!   order. Adapter construction stays in the caller because only the REPL
//!   path consumes adapters.
//! - [`teardown`] -- mirror of the cleanup tail: graceful MCP shutdowns
//!   (drop adapters first so [`Arc::try_unwrap`] succeeds), then stop the
//!   container, then finalize the session row. Errors are logged and never
//!   override the caller's outcome.
//!
//! The MCP children are `podman exec` processes whose stdio rides through
//! the container; tearing the container down before shutting them down
//! races their pipes, so the order in [`teardown`] is load-bearing.
//!
//! [`run`]: crate::cli::run

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::cli::env_arg::CliEnvEntries;
use crate::config::{Config, ContainerConfig, McpServerSpec};
use crate::container::{
    Container, ContainerLaunchSpec, ContainerMount, ContainerWorkspace, embedded,
};
use crate::error::{OutrigError, Result};
use crate::image::{self, ImageTag};
use crate::llm;
use crate::mcp::McpClient;
use crate::process::Transcript;
use crate::repo;
use crate::session::{self, Session, SessionId, SessionStore};

pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct ProgressSpan {
    started: Instant,
}

impl ProgressSpan {
    pub(crate) fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        eprintln!("[outrig] {label}");
        Self {
            started: Instant::now(),
        }
    }

    pub(crate) fn done(self, message: impl AsRef<str>) {
        eprintln!(
            "[outrig] {} ({})",
            message.as_ref(),
            format_elapsed(self.started.elapsed())
        );
    }
}

pub(crate) fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn format_elapsed(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let secs = duration.as_secs();
    if secs < 60 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    format!("{}m{:02}s", secs / 60, secs % 60)
}

fn resolve_workspace_host(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

/// Inputs to [`setup`]. Borrowed to keep the call site cheap; the lifetime
/// is the caller's stack frame.
pub struct SessionSetupArgs<'a> {
    pub repo_cfg_path: &'a Path,
    pub global_cfg_path: &'a Path,
    pub session_root_flag: Option<&'a Path>,
    pub container_flag: Option<&'a str>,
    /// Existing session id or podman container name to attach to instead of
    /// starting a fresh container. Used by `outrig mcp --attach`.
    pub attach_target: Option<&'a str>,
    /// Raw `--agent` flag. Read only when `require_agent = true`; ignored
    /// otherwise (and `outrig mcp` always passes `None`).
    pub agent_flag: Option<&'a str>,
    /// `true` for `outrig run`: [`setup`] resolves an agent from
    /// `agent_flag.or(cfg.default_agent)` (errors if neither) and lets
    /// `agent.container` participate in the container fallback.
    /// `false` for `outrig mcp`: no agent at all -- `llm::resolve_agent` is
    /// not called, `cfg.default_agent` is not consulted, the resulting
    /// [`Session::agent_name`] is `None`, and the container cascade is
    /// `container_flag -> default_container` only.
    pub require_agent: bool,
    pub explicit_session_dir: Option<&'a Path>,
    pub verbose: u8,
}

/// Output of [`setup`]: every long-lived value the post-setup pipeline
/// needs (REPL build, MCP children, teardown). The container is already
/// started + bootstrapped; the session row is already on disk.
pub struct SessionSetup {
    pub cfg: Config,
    pub container_cfg_name: String,
    pub container_cfg: ContainerConfig,
    pub image_tag: ImageTag,
    pub container: Container,
    pub sid: SessionId,
    pub session: Session,
    pub session_dir: PathBuf,
    pub log_dir: PathBuf,
    pub store: SessionStore,
    pub attached: bool,
}

#[derive(Debug)]
struct AttachResolution {
    container_name: String,
    container_cfg_name: String,
}

/// Run the shared bootstrap. Returns once the container is up, the runtime
/// user is bootstrapped, and the session directory + log dir exist.
pub async fn setup(args: SessionSetupArgs<'_>) -> Result<SessionSetup> {
    let repo_root = repo::repo_root_from_config_path(args.repo_cfg_path);
    let span = ProgressSpan::start("loading config");
    let cfg = Config::load(&repo_root, Some(args.global_cfg_path))?;
    span.done("config loaded");

    let session_root =
        session::resolve_session_root(args.session_root_flag, &cfg, &repo::default_session_root());
    let store = SessionStore::new(session_root);
    let attach = match args.attach_target {
        Some(target) if args.require_agent => {
            return Err(OutrigError::Configuration(format!(
                "--attach {target:?} is only supported by `outrig mcp`"
            )));
        }
        Some(target) => Some(resolve_attach_target(target, args.container_flag, &store)?),
        None => None,
    };

    // Agent presence is checked before any container work so the failure
    // mode is identical for `outrig run` regardless of which container
    // would have been picked. `outrig mcp` opts out via `require_agent =
    // false` -- it has no agent concept, so `agent_flag` and
    // `cfg.default_agent` are not consulted at all.
    let span = ProgressSpan::start("resolving agent and container");
    let (session_agent_name, agent_container) = if attach.is_some() {
        (None, None)
    } else if args.require_agent {
        let agent_name = args
            .agent_flag
            .or(cfg.default_agent.as_deref())
            .ok_or_else(|| {
                OutrigError::Configuration("no --agent and no default-agent configured".to_string())
            })?;
        let resolved = llm::resolve_agent(&cfg, agent_name)?;
        (
            Some(resolved.agent_name.clone()),
            resolved.container.clone(),
        )
    } else {
        (None, None)
    };

    let container_cfg_name = match &attach {
        Some(attach) => attach.container_cfg_name.clone(),
        None => args
            .container_flag
            .or(agent_container.as_deref())
            .or(cfg.default_container.as_deref())
            .ok_or_else(|| {
                let msg = if args.require_agent {
                    "no --container, agent.container, or default-container configured"
                } else {
                    "no --container or default-container configured"
                };
                OutrigError::Configuration(msg.to_string())
            })?
            .to_string(),
    };
    let container_cfg = cfg
        .containers
        .get(&container_cfg_name)
        .ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container-config {container_cfg_name:?} does not match any [containers.<name>]"
            ))
        })?
        .clone();
    if let Some(attach) = &attach {
        span.done(format!(
            "attach target resolved: container {}, container-config {}",
            attach.container_name, container_cfg_name
        ));
    } else if let Some(agent) = &session_agent_name {
        span.done(format!(
            "agent/container resolved: agent {agent}, container {container_cfg_name}"
        ));
    } else {
        span.done(format!("container resolved: {container_cfg_name}"));
    }

    let image_tag = if let Some(attach) = &attach {
        let span = ProgressSpan::start(format!(
            "inspecting attached container {}",
            attach.container_name
        ));
        let inspect = Container::inspect_existing(&attach.container_name, None).await?;
        if !inspect.running {
            return Err(OutrigError::Configuration(format!(
                "attached container {:?} is not running",
                attach.container_name
            )));
        }
        span.done(format!(
            "attached container ready: {}",
            attach.container_name
        ));
        inspect.image_tag
    } else {
        let span = ProgressSpan::start("computing image tag");
        let image_tag =
            image::compute_tag_for(&container_cfg_name, &container_cfg, &repo_root).await?;
        span.done(format!("image tag computed: {image_tag}"));
        image_tag
    };

    let host_workspace = resolve_workspace_host(&repo_root, &cfg.workspace.host_path);
    let container_workspace = cfg.workspace.container_path.clone();
    let launch = ContainerLaunchSpec {
        workspace: Some(ContainerWorkspace {
            host: host_workspace.clone(),
            container: container_workspace.clone(),
        }),
        mounts: cfg
            .workspace
            .mounts
            .iter()
            .map(|mount| ContainerMount {
                host: resolve_workspace_host(&repo_root, &mount.host_path),
                container: mount.container_path.clone(),
                access: mount.access,
            })
            .collect(),
    };

    if let Some(p) = args.explicit_session_dir
        && !p.is_dir()
    {
        return Err(OutrigError::Configuration(format!(
            "--session-dir {} is not an existing directory (create it first or omit the flag)",
            p.display()
        )));
    }

    let sid = SessionId::new();
    let container_name = attach
        .as_ref()
        .map(|attach| attach.container_name.clone())
        .unwrap_or_else(|| format!("outrig-{sid}"));
    let mut session = Session {
        id: sid.clone(),
        started_at: SystemTime::now(),
        ended_at: None,
        container_name: container_name.clone(),
        image_tag: image_tag.to_string(),
        container_config_name: container_cfg_name.clone(),
        agent_name: session_agent_name,
        working_dir: repo_root.clone(),
        session_dir: PathBuf::new(), // set by `create` below
        exit_code: None,
        link_target: None,
    };
    let session_dir = store.create(&sid, args.explicit_session_dir, &mut session)?;
    let log_dir = session_dir.join("logs");
    if let Err(e) = tokio::fs::create_dir_all(&log_dir).await {
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e.into());
    }

    let transcript = if args.verbose > 0 {
        match Transcript::create(&log_dir.join("container.log"), true).await {
            Ok(t) => Some(t),
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e);
            }
        }
    } else {
        None
    };

    let mut container = if let Some(attach) = &attach {
        let span = ProgressSpan::start(format!("attaching to container {}", attach.container_name));
        match Container::is_running(&attach.container_name).await {
            Ok(true) => {
                span.done(format!("attached to container: {}", attach.container_name));
                Container::attach(
                    attach.container_name.clone(),
                    image_tag.clone(),
                    Some((&host_workspace, &container_workspace)),
                    transcript,
                )
            }
            Ok(false) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(OutrigError::Configuration(format!(
                    "attached container {:?} is not running",
                    attach.container_name
                )));
            }
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e);
            }
        }
    } else {
        let span = ProgressSpan::start(format!("ensuring image {image_tag}"));
        let image_outcome = match image::ensure_tagged_image_for(
            &container_cfg_name,
            &container_cfg,
            &repo_root,
            &image_tag,
            false,
            transcript.as_ref(),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e);
            }
        };
        let cache_status = if image_outcome.cache_hit {
            "cache hit"
        } else {
            "built"
        };
        span.done(format!(
            "image ready: {} ({cache_status})",
            image_outcome.tag
        ));

        let span = ProgressSpan::start(format!("starting container {container_name}"));
        match Container::start_named(&image_tag, launch, container_name, transcript).await {
            Ok(container) => {
                span.done(format!("container ready: {}", container.name));
                container
            }
            Err(e) => {
                let _ = store.finalize(&sid, SystemTime::now(), 1);
                return Err(e);
            }
        }
    };

    let span = ProgressSpan::start("bootstrapping container user");
    if let Err(e) = container.bootstrap_user().await {
        let _ = container.stop(STOP_GRACE).await;
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e);
    }
    span.done("container user ready");

    Ok(SessionSetup {
        cfg,
        container_cfg_name,
        container_cfg,
        image_tag,
        container,
        sid,
        session,
        session_dir,
        log_dir,
        store,
        attached: attach.is_some(),
    })
}

fn resolve_attach_target(
    raw: &str,
    container_flag: Option<&str>,
    store: &SessionStore,
) -> Result<AttachResolution> {
    let sid = SessionId::from(raw.to_string());
    let session_entry = store.symlink_path(&sid);
    match fs::symlink_metadata(&session_entry) {
        Ok(_) => {
            let (_, session) = store.get_by_id(&sid)?;
            Ok(AttachResolution {
                container_name: session.container_name,
                container_cfg_name: container_flag
                    .unwrap_or(&session.container_config_name)
                    .to_string(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let container_cfg_name = container_flag.ok_or_else(|| {
                OutrigError::Configuration(format!(
                    "--attach {raw:?} did not match a session; pass --container <name> \
                     to treat it as a podman container name"
                ))
            })?;
            Ok(AttachResolution {
                container_name: raw.to_string(),
                container_cfg_name: container_cfg_name.to_string(),
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// Read image-embedded MCP config and overlay explicit `config.toml` entries.
pub async fn merged_mcp(
    container: &Container,
    container_cfg: &ContainerConfig,
) -> Result<BTreeMap<String, McpServerSpec>> {
    let span = ProgressSpan::start("reading and merging MCP configuration");
    let mcp = embedded::merged_mcp(container, &container_cfg.mcp).await?;
    let server_word = plural(mcp.len(), "server", "servers");
    span.done(format!(
        "MCP configuration ready: {} {server_word}",
        mcp.len()
    ));
    Ok(mcp)
}

/// Spawn one [`McpClient`] per backing MCP declared in `mcp`, in key-sorted
/// (`BTreeMap`) iteration order. `cli_env` provides any `--env` overlay
/// entries to merge per server. Adapter construction is the caller's job
/// because only the REPL path consumes adapters.
pub async fn connect_mcp_clients(
    container: &Container,
    mcp: &BTreeMap<String, McpServerSpec>,
    log_dir: &Path,
    cli_env: &CliEnvEntries,
) -> Result<Vec<Arc<McpClient>>> {
    let mut arcs = Vec::with_capacity(mcp.len());
    for (mcp_name, spec) in mcp {
        let span = ProgressSpan::start(format!("MCP {mcp_name}: initializing"));
        let extra_env = cli_env.for_server(mcp_name);
        let client =
            McpClient::connect_via_podman_exec(container, spec, mcp_name, log_dir, &extra_env)
                .await?;
        span.done(format!("MCP {mcp_name}: initialized"));
        arcs.push(Arc::new(client));
    }
    Ok(arcs)
}

/// Cleanup tail. Order: MCP shutdowns (so their `podman exec` pipes drain
/// before the container goes away) -> container stop -> session finalize.
/// Each step's failure is logged but never propagated; the caller's outcome
/// owns the process exit code.
///
/// Callers must drop any `Arc<McpClient>` clones (e.g. tool adapters) and
/// the agent before invoking this -- otherwise [`Arc::try_unwrap`] returns
/// `Err` and the explicit `shutdown` is skipped in favor of `Drop`.
pub async fn teardown(
    mcp_arcs: Vec<Arc<McpClient>>,
    container: Container,
    store: &SessionStore,
    sid: &SessionId,
    final_exit: i32,
) {
    for arc in mcp_arcs {
        match Arc::try_unwrap(arc) {
            Ok(client) => {
                if let Err(e) = client.shutdown().await {
                    tracing::warn!(
                        target: "outrig::cli::session_setup",
                        "mcp shutdown failed: {e}"
                    );
                }
            }
            Err(_) => {
                tracing::warn!(
                    target: "outrig::cli::session_setup",
                    "mcp client still has outstanding refs at cleanup; relying on Drop"
                );
            }
        }
    }
    if let Err(e) = container.stop(STOP_GRACE).await {
        tracing::warn!(
            target: "outrig::cli::session_setup",
            "container stop failed: {e}"
        );
    }
    if let Err(e) = store.finalize(sid, SystemTime::now(), final_exit) {
        tracing::warn!(
            target: "outrig::cli::session_setup",
            "session finalize failed: {e}"
        );
    }
}
