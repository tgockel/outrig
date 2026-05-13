//! `outrig mcp` orchestrator: wire the shared `SessionSetup` bootstrap to a
//! [`ProxyServer`] served over rmcp's stdio transport. Runs as a server (not
//! a REPL): the external client speaks JSON-RPC on the binary's stdout, and
//! everything else (banner, tracing) goes to stderr.
//!
//! The exit triggers -- stdin EOF (peer disconnect), SIGINT, SIGTERM -- all
//! funnel through the same teardown order as `outrig run`: cancel the rmcp
//! service so its dispatcher quiesces -> `McpClient::shutdown` per backing
//! server -> `Container::stop` -> `SessionStore::finalize`. Backing MCPs
//! are `podman exec` processes whose pipes ride through the container, so
//! tearing the container down before stopping the rmcp service races them.

#![deny(clippy::print_stdout)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use clap::{ArgAction, Parser, Subcommand};
use serde::Serialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cli::env_arg::CliEnvEntries;
use crate::cli::session_setup::{self, SessionSetup, SessionSetupArgs};
use crate::config::{ContainerConfig, McpServerSpec};
use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::image::ImageTag;
use crate::mcp::McpClient;
use crate::mcp_proxy::ProxyServer;

const ATTACH_MONITOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Parser)]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: Option<McpCommand>,

    /// Pick a `[containers.<name>]` block. Falls back to top-level
    /// `default-container` only -- `outrig mcp` has no agent, so there is no
    /// `agent.container` to consult.
    #[arg(long, global = true, value_name = "NAME")]
    pub container: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", global = true, value_name = "PATH")]
    pub session_dir: Option<PathBuf>,

    /// Attach to an existing outrig session id or podman container name
    /// instead of starting a fresh container.
    #[arg(long, global = true, value_name = "SESSION_OR_CONTAINER")]
    pub attach: Option<String>,

    /// Add or override env vars for MCP servers. Repeatable.
    /// `KEY=VALUE` applies to every server; `SERVER:KEY=VALUE` targets one.
    #[arg(long = "env", global = true, value_name = "KEY=VALUE", action = ArgAction::Append)]
    pub env: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Serve OutRig's self-description tools over stdio.
    #[command(name = "self")]
    SelfDescription,
    /// Print the image/config merged MCP table and exit.
    ShowMerged,
}

impl McpArgs {
    pub fn is_self_description(&self) -> bool {
        matches!(self.cmd, Some(McpCommand::SelfDescription))
    }
}

/// Run one `outrig mcp` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &McpArgs,
    verbose: u8,
) -> Result<i32> {
    let cli_env =
        CliEnvEntries::parse(&args.env).map_err(|e| OutrigError::Configuration(e.to_string()))?;

    let setup = session_setup::setup(SessionSetupArgs {
        repo_cfg_path,
        global_cfg_path,
        session_root_flag,
        container_flag: args.container.as_deref(),
        attach_target: args.attach.as_deref(),
        agent_flag: None,
        require_agent: false,
        explicit_session_dir: args.session_dir.as_deref(),
        verbose,
    })
    .await?;

    match &args.cmd {
        Some(McpCommand::SelfDescription) => unreachable!("handled before repo context"),
        None => serve(setup, cli_env).await,
        Some(McpCommand::ShowMerged) => show_merged(setup).await,
    }
}

async fn serve(setup: SessionSetup, cli_env: CliEnvEntries) -> Result<i32> {
    let SessionSetup {
        container_cfg_name,
        container_cfg,
        image_tag,
        container,
        sid,
        log_dir,
        store,
        attached,
        cfg: _,
        session: _,
        session_dir: _,
    } = setup;

    // Validate per-server env entries against the resolved MCP map.
    let mcp = session_setup::merged_mcp(&container, &container_cfg).await?;
    for name in cli_env.per_server_names() {
        if !mcp.contains_key(name) {
            return Err(OutrigError::Configuration(format!(
                "--env {name}:...: container '{}' has no MCP server '{name}'",
                container_cfg_name
            )));
        }
    }

    let mut mcp_arcs: Vec<Arc<McpClient>> = Vec::new();
    let outcome: Result<i32> = serve_inner(
        &container_cfg_name,
        &image_tag,
        &container,
        &log_dir,
        sid.as_str(),
        &mut mcp_arcs,
        &mcp,
        &cli_env,
        attached,
    )
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(mcp_arcs, container, &store, &sid, final_exit).await;
    if attached
        && outcome
            .as_ref()
            .err()
            .is_some_and(is_attached_container_stopped)
    {
        eprintln!(
            "error: {}",
            outcome.as_ref().expect_err("checked err above")
        );
        std::process::exit(final_exit.clamp(0, 255));
    }
    outcome
}

fn is_attached_container_stopped(err: &OutrigError) -> bool {
    matches!(err, OutrigError::Configuration(msg) if msg.contains("attached container") && msg.contains("stopped"))
}

async fn show_merged(setup: SessionSetup) -> Result<i32> {
    let SessionSetup {
        container_cfg,
        container,
        sid,
        store,
        attached: _,
        cfg: _,
        container_cfg_name: _,
        image_tag: _,
        session: _,
        session_dir: _,
        log_dir: _,
    } = setup;

    let outcome = show_merged_inner(&container_cfg, &container).await;
    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(Vec::new(), container, &store, &sid, final_exit).await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn serve_inner(
    container_cfg_name: &str,
    image_tag: &ImageTag,
    container: &Container,
    log_dir: &Path,
    session_id: &str,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
    mcp: &BTreeMap<String, McpServerSpec>,
    cli_env: &CliEnvEntries,
    attached: bool,
) -> Result<i32> {
    let connected = session_setup::connect_mcp_clients(container, mcp, log_dir, cli_env).await?;
    if connected.is_empty() {
        return Err(OutrigError::Configuration(
            "outrig mcp with no merged MCP entries has nothing to proxy".to_string(),
        ));
    }
    mcp_arcs.extend(connected);

    let proxy = ProxyServer::build(mcp_arcs.clone()).await?;
    let per_server_counts: Vec<(String, usize)> = proxy
        .per_server_counts()
        .into_iter()
        .map(|(n, c)| (n.to_string(), c))
        .collect();
    let public_names: Vec<String> = proxy.iter_public_names().map(str::to_string).collect();

    print_banner(
        container_cfg_name,
        image_tag,
        &container.name,
        &per_server_counts,
        &public_names,
        session_id,
        attached,
    );

    // `serve_server_with_ct` lets us hold the cancellation token outside the
    // service, which is otherwise consumed by `waiting()`. Cancel-on-signal
    // -> dispatcher quiesces -> `waiting()` returns -> teardown runs.
    let ct = CancellationToken::new();
    let service =
        rmcp::service::serve_server_with_ct(proxy, rmcp::transport::stdio(), ct.clone()).await?;
    eprintln!("[outrig] mcp server ready");

    let mut waiter = tokio::spawn(service.waiting());
    let mut sigterm = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    let mut monitor = Box::pin(async {
        if attached {
            wait_for_attached_container_stop(container.name.clone()).await
        } else {
            std::future::pending::<Result<()>>().await
        }
    });

    tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGINT; shutting down");
            ct.cancel();
        }
        _ = sigterm.recv() => {
            tracing::info!(target: "outrig::cli::mcp", "received SIGTERM; shutting down");
            ct.cancel();
        }
        result = &mut waiter => {
            log_waiter_result(result);
            return Ok(0);
        }
        result = &mut monitor => {
            ct.cancel();
            match tokio::time::timeout(ATTACH_MONITOR_SHUTDOWN_GRACE, &mut waiter).await {
                Ok(waiter_result) => log_waiter_result(waiter_result),
                Err(_) => {
                    waiter.abort();
                    tracing::warn!(
                        target: "outrig::cli::mcp",
                        "rmcp service did not stop after attached container disappeared"
                    );
                }
            }
            return match result {
                Ok(()) => Err(OutrigError::Configuration(
                    "attached container monitor ended unexpectedly".to_string(),
                )),
                Err(e) => Err(e),
            };
        }
    }

    // Signal path: wait for the service to wind down after cancellation.
    let result = waiter.await;
    log_waiter_result(result);
    Ok(0)
}

async fn wait_for_attached_container_stop(container_name: String) -> Result<()> {
    let mut child = tokio::process::Command::new("podman")
        .arg("wait")
        .arg(&container_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let status = child.wait().await?;
    if !status.success() {
        tracing::warn!(
            target: "outrig::cli::mcp",
            "podman wait for attached container {container_name:?} exited with {status}"
        );
    }
    Err(OutrigError::Configuration(format!(
        "attached container {container_name:?} stopped while `outrig mcp` was attached"
    )))
}

async fn show_merged_inner(container_cfg: &ContainerConfig, container: &Container) -> Result<i32> {
    let mcp = session_setup::merged_mcp(container, container_cfg).await?;
    write_merged_mcp(&mcp)?;
    Ok(0)
}

fn write_merged_mcp(mcp: &BTreeMap<String, McpServerSpec>) -> Result<()> {
    #[derive(Serialize)]
    struct MergedMcpView<'a> {
        mcp: &'a BTreeMap<String, McpServerSpec>,
    }

    let rendered = if mcp.is_empty() {
        "[mcp]\n".to_string()
    } else {
        toml::to_string_pretty(&MergedMcpView { mcp }).map_err(|source| {
            OutrigError::Configuration(format!("serialize merged MCP TOML: {source}"))
        })?
    };

    let mut stdout = std::io::stdout().lock();
    stdout.write_all(rendered.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn log_waiter_result(
    result: std::result::Result<
        std::result::Result<rmcp::service::QuitReason, tokio::task::JoinError>,
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok(Ok(reason)) => {
            tracing::debug!(
                target: "outrig::cli::mcp",
                "rmcp service exited: {reason:?}"
            );
        }
        Ok(Err(join_err)) => {
            tracing::warn!(
                target: "outrig::cli::mcp",
                "rmcp dispatcher join error: {join_err}"
            );
        }
        Err(join_err) => {
            tracing::warn!(
                target: "outrig::cli::mcp",
                "rmcp waiter join error: {join_err}"
            );
        }
    }
}

fn print_banner(
    container_name: &str,
    image_tag: &ImageTag,
    container_pod_name: &str,
    per_server_counts: &[(String, usize)],
    public_names: &[String],
    session_id: &str,
    attached: bool,
) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] container-config:  {container_name}");
    let _ = writeln!(buf, "[outrig] image:             {image_tag}");
    let container_action = if attached { "attached" } else { "started" };
    let _ = writeln!(
        buf,
        "[outrig] container {container_action}: {container_pod_name}"
    );
    for (name, count) in per_server_counts {
        let plural = if *count == 1 { "tool" } else { "tools" };
        let _ = writeln!(buf, "[outrig] mcp {name}: initialized ({count} {plural})");
    }
    let names_joined = public_names
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(buf, "[outrig] tools available: {names_joined}");
    let _ = writeln!(buf, "[outrig] session id: {session_id}");
    let _ = writeln!(buf, "[outrig] transport: stdio");
    eprint!("{buf}");
}
