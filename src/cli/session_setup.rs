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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::config::{Config, ContainerConfig, McpServerSpec};
use crate::container::{Container, embedded};
use crate::error::{OutrigError, Result};
use crate::image::{self, ImageTag};
use crate::llm;
use crate::mcp::McpClient;
use crate::process::Transcript;
use crate::repo;
use crate::session::{self, Session, SessionId, SessionStore};

pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);

/// Inputs to [`setup`]. Borrowed to keep the call site cheap; the lifetime
/// is the caller's stack frame.
pub struct SessionSetupArgs<'a> {
    pub repo_cfg_path: &'a Path,
    pub global_cfg_path: &'a Path,
    pub session_root_flag: Option<&'a Path>,
    pub container_flag: Option<&'a str>,
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
}

/// Run the shared bootstrap. Returns once the container is up, the runtime
/// user is bootstrapped, and the session directory + log dir exist.
pub async fn setup(args: SessionSetupArgs<'_>) -> Result<SessionSetup> {
    let repo_root = repo::repo_root_from_config_path(args.repo_cfg_path);
    let cfg = Config::load(&repo_root, Some(args.global_cfg_path))?;

    // Agent presence is checked before any container work so the failure
    // mode is identical for `outrig run` regardless of which container
    // would have been picked. `outrig mcp` opts out via `require_agent =
    // false` -- it has no agent concept, so `agent_flag` and
    // `cfg.default_agent` are not consulted at all.
    let (session_agent_name, agent_container) = if args.require_agent {
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

    let container_cfg_name = args
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
        .to_string();
    let container_cfg = cfg
        .containers
        .get(&container_cfg_name)
        .ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container-config {container_cfg_name:?} does not match any [containers.<name>]"
            ))
        })?
        .clone();

    let image_tag = image::compute_tag_for(&container_cfg_name, &container_cfg, &repo_root).await?;

    let host_workspace = if cfg.workspace.host_path.is_absolute() {
        cfg.workspace.host_path.clone()
    } else {
        repo_root.join(&cfg.workspace.host_path)
    };
    let container_workspace = cfg.workspace.container_path.clone();

    if let Some(p) = args.explicit_session_dir
        && !p.is_dir()
    {
        return Err(OutrigError::Configuration(format!(
            "--session-dir {} is not an existing directory (create it first or omit the flag)",
            p.display()
        )));
    }

    let sid = SessionId::new();
    let container_name = format!("outrig-{sid}");
    let session_root =
        session::resolve_session_root(args.session_root_flag, &cfg, &repo::default_session_root());
    let store = SessionStore::new(session_root);
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

    if let Err(e) = image::ensure_tagged_image_for(
        &container_cfg_name,
        &container_cfg,
        &repo_root,
        &image_tag,
        false,
        transcript.as_ref(),
    )
    .await
    {
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e);
    }

    let mut container = match Container::start_named(
        &image_tag,
        Some((&host_workspace, &container_workspace)),
        container_name,
        transcript,
    )
    .await
    {
        Ok(container) => container,
        Err(e) => {
            let _ = store.finalize(&sid, SystemTime::now(), 1);
            return Err(e);
        }
    };

    if let Err(e) = container.bootstrap_user().await {
        let _ = container.stop(STOP_GRACE).await;
        let _ = store.finalize(&sid, SystemTime::now(), 1);
        return Err(e);
    }

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
    })
}

/// Read image-embedded MCP config and overlay explicit `config.toml` entries.
pub async fn merged_mcp(
    container: &Container,
    container_cfg: &ContainerConfig,
) -> Result<BTreeMap<String, McpServerSpec>> {
    embedded::merged_mcp(container, &container_cfg.mcp).await
}

/// Spawn one [`McpClient`] per backing MCP declared in `mcp`, in key-sorted
/// (`BTreeMap`) iteration order. Adapter construction is the caller's job
/// because only the REPL path consumes adapters.
pub async fn connect_mcp_clients(
    container: &Container,
    mcp: &BTreeMap<String, McpServerSpec>,
    log_dir: &Path,
) -> Result<Vec<Arc<McpClient>>> {
    let mut arcs = Vec::with_capacity(mcp.len());
    for (mcp_name, spec) in mcp {
        let client = McpClient::connect_via_podman_exec(container, spec, mcp_name, log_dir).await?;
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
