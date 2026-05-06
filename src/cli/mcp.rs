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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::cli::session_setup::{self, SessionSetup, SessionSetupArgs};
use crate::config::ContainerConfig;
use crate::container::Container;
use crate::error::{OutrigError, Result};
use crate::image::ImageTag;
use crate::mcp::McpClient;
use crate::mcp_proxy::ProxyServer;

#[derive(Debug, Parser)]
pub struct McpArgs {
    /// Pick a `[containers.<name>]` block. Falls back to top-level
    /// `default-container` only -- `outrig mcp` has no agent, so there is no
    /// `agent.container` to consult.
    #[arg(long, value_name = "NAME")]
    pub container: Option<String>,

    /// Write the session into an explicit, already-existing directory. The
    /// session root gets a symlink at `<root>/<sid>` pointing at this path.
    #[arg(long = "session-dir", value_name = "PATH")]
    pub session_dir: Option<PathBuf>,
}

/// Run one `outrig mcp` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    session_root_flag: Option<&Path>,
    args: &McpArgs,
) -> Result<i32> {
    let setup = session_setup::setup(SessionSetupArgs {
        repo_cfg_path,
        global_cfg_path,
        session_root_flag,
        container_flag: args.container.as_deref(),
        agent_flag: None,
        require_agent: false,
        explicit_session_dir: args.session_dir.as_deref(),
    })
    .await?;

    let SessionSetup {
        container_cfg_name,
        container_cfg,
        image_tag,
        container,
        sid,
        log_dir,
        store,
        cfg: _,
        session: _,
        session_dir: _,
    } = setup;

    let mut mcp_arcs: Vec<Arc<McpClient>> = Vec::new();
    let outcome: Result<i32> = serve_inner(
        &container_cfg_name,
        &container_cfg,
        &image_tag,
        &container,
        &log_dir,
        sid.as_str(),
        &mut mcp_arcs,
    )
    .await;

    let final_exit = outcome.as_ref().copied().unwrap_or(1);
    session_setup::teardown(mcp_arcs, container, &store, &sid, final_exit).await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn serve_inner(
    container_cfg_name: &str,
    container_cfg: &ContainerConfig,
    image_tag: &ImageTag,
    container: &Container,
    log_dir: &Path,
    session_id: &str,
    mcp_arcs: &mut Vec<Arc<McpClient>>,
) -> Result<i32> {
    let connected = session_setup::connect_mcp_clients(container, container_cfg, log_dir).await?;
    if connected.is_empty() {
        return Err(OutrigError::Configuration(
            "outrig mcp with no `[containers.<name>.mcp]` entries has nothing to proxy".to_string(),
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
    );

    // `serve_server_with_ct` lets us hold the cancellation token outside the
    // service, which is otherwise consumed by `waiting()`. Cancel-on-signal
    // -> dispatcher quiesces -> `waiting()` returns -> teardown runs.
    let ct = CancellationToken::new();
    let service =
        rmcp::service::serve_server_with_ct(proxy, rmcp::transport::stdio(), ct.clone()).await?;
    eprintln!("[outrig] mcp server ready");

    let waiter = tokio::spawn(service.waiting());
    let mut sigterm = signal(SignalKind::terminate()).map_err(OutrigError::Io)?;
    tokio::pin!(waiter);

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
    }

    // Signal path: wait for the service to wind down after cancellation.
    let result = waiter.await;
    log_waiter_result(result);
    Ok(0)
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
) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] container-config:  {container_name}");
    let _ = writeln!(buf, "[outrig] image:             {image_tag}");
    let _ = writeln!(buf, "[outrig] container started: {container_pod_name}");
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
